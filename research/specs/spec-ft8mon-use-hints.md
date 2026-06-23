# Algorithm spec: Brute-force directed search via callsign hints (`use_hints`)

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — hint state at lines 422-466, application around
  lines 2706-2758 (inside `one_iter1`), entry signature at lines
  3030-3038 and `ft8.h`
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

The standard soft-demod LLR vector reflects the receiver's noisy
estimate of every bit. When the receiver has prior information about
the likely identity of one of the callsigns in the message — for
example, because pancetta has just finished a QSO with that station,
or because the station has been spotted on a DX cluster, or because
the operator has tagged it as a hunt target — that prior can be folded
into the LLR vector by forcing the corresponding bits to high
confidence. ft8mon implements this as a brute-force directed search:
for each candidate, after the unhinted soft-demod path fails, retry
LDPC + CRC once per hint, with the hinted callsign's 28-bit hash
clamped to ±4.97 in the LLR vector. Each hint costs one additional
BP + OSD decode attempt. If a hint matches the true signal, LDPC
typically succeeds at LDPC `ok_thresh` much weaker than it would
without; if a hint does not match, the CRC catches it before any
false decode is emitted.

ft8mon distinguishes two hint slots:
- **hints1** — applied to bit positions 0..27 (the first callsign
  hash, typically the *transmitting* station).
- **hints2** — applied to bit positions 29..56 (the second callsign
  hash, typically the *addressed* station).

The `use_hints` parameter controls behavior:
- `use_hints = 0` — no hint search.
- `use_hints = 1` — search both hint1 (any value) and hint2 lists.
- `use_hints = 2` — search only hint1 entries with value `2` (the
  "CQ" hash placeholder), i.e. force the message to look like a CQ
  from any hinted station. This is the default.

## Algorithm description (PROSE ONLY)

### Inputs
- A 174-element LLR vector `ll174` already produced by the regular
  soft-demod path for this candidate (single-symbol, then pair, then
  triple — whichever produced the last vector before the hint loop).
- A 79×8 complex per-symbol FFT array `m79` for SNR re-estimation.
- The candidate's `(best_hz, best_off, hz0_for_cb, hz1_for_cb)`.
- A `hints1` list — 28-bit unsigned integers, each one a packed
  representation of a callsign hash. Convention: the special value `2`
  means "the callsign is CQ" (the standard FT8 CQ encoding); other
  values are real callsign hashes computed by the caller.
- A `hints2` list — same format, applied to the second callsign slot.

### Outputs
- Same return convention as `try_decode`: 2 if a brand-new message
  was decoded and the callback returned "subtract this", 1 if a
  duplicate of a previously-decoded message was found, 0 if no hint
  produced a valid decode.

### Steps

1. **Iterate over `hints1`** (when `use_hints` is non-zero). For each
   28-bit hint value `h`:
   - If `use_hints == 2` and `h != 2`, skip this hint. (Restricts the
     directed search to CQ-shaped messages only — the most common
     case for pancetta's autonomous responder and the lowest
     false-positive risk because CQ messages have the most rigid
     structure.)
   - Build a fresh 174-element LLR vector `n174`:
     - For `i` in `0..28`: extract the `(27 - i)`-th bit of `h`
       (most-significant bit first). If the bit is 1, set
       `n174[i] = -4.97`; if 0, set `n174[i] = +4.97`.
     - For `i` in `28..174`: copy from the unhinted `ll174[i]`.
   - Call `try_decode(samples200, n174, best_hz, best_off, ..., use_osd=0, "hint1", m79)`.
     Note `use_osd = 0` — hint decodes do not invoke OSD. The hint is
     already so much information that BP alone should converge if the
     hint matches; if it doesn't match, OSD would just be wasted
     compute (and increases false-positive risk because OSD trusts the
     LLRs harder).
   - If `try_decode` returns non-zero (decode succeeded + CRC passed),
     return that result immediately. No further hints tried for this
     candidate.

2. **Iterate over `hints2`** (only when `use_hints == 1`, not when
   `use_hints == 2`). Same loop structure as above, but the hint bits
   land at positions `29..57` of the LLR vector instead of `0..28`.
   Bit positions 28 and 57 (the i3/i4 bits separating the callsign
   slots in the FT8 message layout) are left untouched.

3. **Return 0** if no hint produced a decode.

### How this is *used* by the calling code

`use_hints` is the last block in `one_iter1`, after `soft_ones`,
`soft_pairs`, and `soft_triples` have all failed. It is purely
additive: it produces extra decode attempts using already-available
data (the `ll174` and `m79` from the previous step), at the cost of M
additional BP+CRC decodes per candidate (where M is the hint count).
A hint that doesn't match the candidate produces no decode; a hint
that matches turns a marginal weak signal into a clean decode.

The 28-bit width is not arbitrary: it matches the standard FT8
callsign hash size (the 28-bit packed representation that the FT8
protocol uses for standard callsigns in i3=1 messages). The ±4.97
LLR value matches a Bayesian "almost certain" prior; ft8mon uses 4.97
elsewhere as the saturation value for the LLR scale.

### Numerical constants (facts, not expression)
- `use_hints = 2` is the default — CQ-only hints.
- LLR hint clamp value: ±4.97 (positive when bit = 0, negative when
  bit = 1).
- Hint1 bit slot: LLR indices 0..27 (first callsign hash).
- Hint2 bit slot: LLR indices 29..56 (second callsign hash).
- Bit index 28 (i3) and 57 (i4) are protocol-defined and left
  untouched in `ll174`.
- Special hint value: `2` = "this is a CQ-shaped message".
- Sign convention: positive LLR favors bit = 0, negative favors
  bit = 1. (Same convention as the rest of the LLR pipeline.)

### Edge cases
- **Empty hint lists** — the loops simply don't execute, and the
  function returns 0 immediately. No special-casing needed.
- **Hint termination** — the C interface uses a sentinel-terminated
  array (the loop runs `while hints1[i]`), so the list is implicitly
  null-terminated. In a Rust port, a `Vec<u32>` or `&[u32]` is
  cleaner.
- **Hint collisions** — multiple hints with the same value just retry
  the same decode; benign but wasteful. The caller should dedupe
  before passing the list in.
- **use_hints == 2 filter on hints2** — only `hints1` is gated by the
  "CQ only" filter, and `hints2` is skipped entirely under
  `use_hints == 2`. This is asymmetric: CQ messages don't have a
  meaningful second callsign (it's just CQ + grid), so hinting the
  second slot would be ill-defined.
- **OSD off for hints** — `use_osd = 0` is non-negotiable. Hinting +
  OSD is structurally dangerous: OSD picks an extended set of
  hypotheses to flip based on LLR magnitude, and a fully-clamped hint
  region locks half the bits to ±4.97. OSD has no degrees of freedom
  in the hinted region and may emit false codewords for the unhinted
  region.
- **Hash false-positive rate** — 28-bit hash space gives ~2.7 × 10⁸
  possible values. For typical hint list sizes (10-1000 entries),
  random hash collision with the wrong codeword is very rare;
  protection is the LDPC + 14-bit CRC.

## Conflict with pancetta's existing mechanisms

Pancetta has rich callsign context that is currently not used during
decode:
- The QSO state machine knows the current DX callsign (the station
  pancetta is in mid-QSO with).
- The autonomous operator has a list of currently-being-hunted
  callsigns from the cqdx.io feed.
- The DX cluster spotter feed names recently-active stations on the
  band.

All three are natural hint sources. The expected coverage win lives
specifically in the weak-SNR / slot-edge / capture-effect buckets
(per hb-217 batch notes) — exactly where the unhinted LLR vector is
right at the LDPC convergence threshold. ft8mon's structure suggests
two natural pancetta-side wiring points:

1. **Mid-QSO hint** — during a QSO, push the DX callsign's hash into
   `hints2` so any inbound message addressed to the pancetta
   operator's call from that DX gets a +4.97 boost on positions
   29..56. Lifetime: from QSO start to QSO end (RR73 emitted).
2. **Hunt-list hints** — every cqdx.io spot and every recently-
   decoded callsign goes into a rolling `hints1` list with TTL of
   ~60 seconds. `use_hints = 2` (CQ-only) is the safest default
   because hunt-targets are nearly always heard via CQ before pancetta
   responds; expanding to `use_hints = 1` only after hunt-list quality
   is well-measured.

Pancetta-specific risk: the autonomous responder already has a
"recently-responded-to" 60-second back-off per callsign (per
`autonomous.rs` per CLAUDE.md). Hinting recently-decoded callsigns
will cause them to keep appearing in the decoded stream; that
back-off list correctly suppresses the spurious response. No new
suppression logic needed.

Interaction with hb-058 (slash-R-suffix FP filter) and hb-103
(content-based FP score): hint decodes still flow through the same FP
filters because they go through `try_decode` and then the standard
callback. Hint-decoded messages may have slightly different feature
distributions (e.g. more concentrated in the high-confidence tail of
content_score), so the existing thresholds may need recalibration if
hint decodes are a significant fraction of the emitted set.

## Estimated Rust port effort
- ~100-150 LOC in `pancetta-ft8/src/decoder/` plus ~50 LOC in
  `pancetta-qso/` or `pancetta/src/coordinator/` for the hint-list
  glue.
- 1-2 sessions: (S1) implement the LLR clamping helper and the
  per-hint retry loop, with a synthetic test that a weak signal
  decoded against a matching hint succeeds while the same signal
  against the wrong hint fails CRC; (S2) wire the QSO state machine
  and hunt list into a `Hints` struct passed to the decoder, eval on
  hard-200 with simulated mid-QSO scenarios.

## Implementation notes for the implementer thread

- Suggested abstraction:
  ```text
  struct Hints {
      cq_callsigns: Vec<u32>,      // 28-bit packed hashes
      response_callsigns: Vec<u32>,
      mode: HintMode,              // CqOnly | All | Off
  }
  ```
  The decoder reads this struct and applies the loop described above.
- The 28-bit callsign hash is the same packed representation used by
  the FT8 message encoder for standard callsigns. Pancetta's
  `pancetta-ft8/src/message.rs` already has the pack function (look
  for the i3=1 path); reuse it to compute hints from a callsign string.
  Do not invent a new hash.
- The clamp value ±4.97 should be a named constant
  (`HINT_LLR_CLAMP: f32 = 4.97`). Bayesian-justified: with the LLR
  scale ft8mon uses, ±4.97 corresponds to ~99.3% confidence on the
  bit. This is the same value ft8mon uses elsewhere as the saturation.
- OSD must be disabled on hint attempts. In pancetta's decoder, this
  is the same flag that controls `osd_depth` per `Ft8Config`. The
  hint loop should temporarily pass `0` regardless of the config value.
- Bit indexing: ft8mon's source uses big-endian bit extraction
  (`h & (1 << 27)` then `h <<= 1`). When porting, pick whichever
  convention matches pancetta's existing bit-packing in `message.rs`
  and confirm with a unit test that round-trips a packed callsign
  through pack → hint-clamp → unpack and recovers the original.
- The hint mechanism touches only the LLR vector. It does not
  re-extract `m79` or re-run sync; it composes cleanly with all
  three soft-demod variants (single / pair / triple) — when one fails,
  apply hints to *that* failed LLR vector.
- Performance: hint count × candidate count × decode-attempt cost. For
  a hunt list of 50 callsigns and ~100 candidates per slot, this is
  5000 extra LDPC+CRC attempts per slot. Each attempt is fast (BP
  only, no OSD), but the total can be 1-2 seconds on Slow tier.
  Recommend `Hints::mode = Off` as the Slow tier default; CqOnly for
  Moderate; configurable for Fast. The hb-216 tier classifier already
  provides this hook.
- Pancetta's QSO state machine should feed the *other* station's hash
  into `response_callsigns` (hints2) — when pancetta sends `K5ARH W1ABC -10`,
  it expects to hear `W1ABC K5ARH RR73`, so W1ABC goes in the
  transmitting-call slot (hints1) and K5ARH in the addressed slot
  (hints2). The directionality matters; getting it wrong wastes hint
  attempts but is otherwise harmless.
- Eval target: simulate the "during QSO" coverage gap from hb-217
  notes — specifically the slot-edge negative-dt bucket where the DX
  callsign is known. Headroom estimate: 200-500 truths recovered per
  hard-200 by hinting at the QSO partner.
