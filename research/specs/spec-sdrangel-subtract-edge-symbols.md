# Algorithm spec: Tapered phantom-symbol modeling at frame edges during spectral subtraction

## Source attribution
- Origin: SDRangel libft8 (Edouard Griffiths, F4EXB), an
  independently-evolved fork of ft8mon (Robert Morris, AB1HL)
- File path: `ft8/ft4.cpp` `subtract()` function (two call sites
  gated by `params.subtract_edge_symbols`) and the
  `FT8Params::subtract_edge_symbols` field declaration in
  `ft8/ft8.h` with the inline comment "model one extra tapered
  symbol at frame start/end during subtraction"
- License: GPL-2.0 (project), GPL-3.0 (libft8 derived from ft8mon)
- Reader date: 2026-06-08

## Why this is a clean delta vs ft8mon

Grep against `rtmrtmrtmrtm/ft8mon` for `subtract_edge_symbols` returns
zero hits. The parameter is SDRangel-side novel work. It guards two
code blocks at the start and end of the spectral-subtraction loop
that model a tapered "phantom" symbol immediately before the first
real symbol and immediately after the last real symbol.

The mechanism is plausible motivated by the ramp-shape used by FT8 /
FT4 transmitters: the first symbol of a transmitted frame is preceded
by a half-symbol-length amplitude ramp (Gaussian-like) that brings
the carrier up from zero. The last symbol is followed by a matching
ramp-down. Existing spectral-subtraction code (ft8mon mainline,
pancetta's current implementation if any) only subtracts the 79 (FT8)
or 103 (FT4) decoded data + Costas symbols proper, leaving the
on-ramp and off-ramp tails un-subtracted in the residual. This
residue:
- biases the noise floor on the next pass,
- may capture-effect-block a weaker overlapping signal whose first
  symbol falls inside the un-subtracted ramp window.

The `subtract_edge_symbols` extension subtracts those edge tails
explicitly by modeling them as one additional "phantom" symbol at
each end, frequency-locked to the decoded frame's first / last tone
and amplitude-tapered with the same ramp shape used by the
within-frame subtraction.

## Purpose

To reduce residual energy at the temporal edges of a successfully
decoded frame so that subsequent decoding passes see cleaner audio in
the vicinity of frame boundaries — improving recall on overlapping
weaker signals whose symbol-time grid does not align with the just-
decoded frame.

This targets the well-known FT8 "capture effect" regime where a
strong signal's frame-edge ramp tails leak into adjacent slot-time
windows and mask weaker signals there. Pancetta's hb-218 capture-
effect work has identified this regime as a ~1058-truth-headroom
target on the hard-200 corpus.

## Algorithm description (PROSE ONLY)

### Inputs
- A buffer of audio samples covering the decode window (the same
  buffer the existing spectral-subtraction stage operates on).
- The corrected per-symbol tone indices `re79[]` for FT8 (or
  `re103[]` for FT4) produced by LDPC + CRC validation of the
  just-decoded frame.
- The decoded frame's `(best_hz, best_off, dt)` triple identifying
  where in the audio buffer the frame lies.
- The `params.subtract_edge_symbols` switch (0 = disabled, the
  default; >0 = enabled).
- The existing `params.subtract_ramp` parameter (default 0.11)
  controlling the fractional length of the ramp transition between
  adjacent symbols, in units of one symbol period.

### Output
- The same audio sample buffer with the modeled signal subtracted,
  including (when enabled) the tapered phantom symbol contributions
  at the leading and trailing frame edges.

### Steps

The standard within-frame subtraction loop iterates over symbols
0..N-1 (N=79 for FT8, N=103 for FT4), at each step reconstructing
the symbol's tone-modulated cosine wave at the corrected tone index
with a ramp transition between adjacent symbols.

The edge-symbol extension adds two extra steps:

1. **Pre-frame phantom symbol** (executed if
   `subtract_edge_symbols > 0` BEFORE the main loop):
   - Model a hypothetical symbol immediately preceding the first
     real symbol, frequency-locked to the **first decoded symbol's
     tone** (`re79[0]` for FT8). The frequency of the phantom
     symbol equals the frequency of symbol 0 — this is the simplest
     and most conservative assumption since the transmitter's
     actual pre-ramp carries no information, and using symbol 0's
     tone produces a smooth phase-continuous extension into the
     ramp-up region.
   - The phantom symbol's amplitude envelope is **only the trailing
     ramp half** — the ramp-up tail that smoothly rises from zero
     to the steady-state amplitude at the start of real symbol 0.
   - Subtract this modeled phantom-symbol contribution from the
     audio buffer.

2. **Post-frame phantom symbol** (executed if
   `subtract_edge_symbols > 0` AFTER the main loop):
   - Symmetric to the pre-frame phantom. Model one hypothetical
     symbol immediately after the last real symbol, frequency-
     locked to the **last decoded symbol's tone** (`re79[N-1]` for
     FT8).
   - Amplitude envelope is **only the leading ramp half** — the
     ramp-down tail that smoothly falls from the steady-state
     amplitude at the end of real symbol N-1 to zero.
   - Subtract from the audio buffer.

The ramp shape — for both within-frame and edge-extension cases —
is the same shape used elsewhere in the SDRangel spectral
subtraction: a Gaussian-like smooth transition of fractional length
`subtract_ramp × block` samples, where `block` is the number of
samples per symbol.

### Numerical constants (facts, not expression)
- `subtract_edge_symbols` default = `0` (disabled). The mechanism is
  opt-in.
- `subtract_ramp` default = `0.11` (i.e. each between-symbol ramp is
  ~11% of one symbol period; the edge-phantom uses this same value).
- Symbol count modeled = `N + 2` when enabled (`N = 79` for FT8,
  `N = 103` for FT4), at the cost of two additional half-ramp
  subtraction kernels per decoded frame.
- The frequency choice for the pre/post phantom is **the boundary
  symbol's tone**, not a band-average or a noise sample. This is the
  smoothest continuation.

### Edge cases
- **Decoded frame lies very close to the audio buffer start or end.**
  The pre-frame phantom symbol may extend past the buffer start.
  Clamp the phantom-symbol subtraction range to the buffer bounds
  before applying; do not subtract past the start of the buffer.
- **`re79[0]` tone is a Costas tone** (it is, always — Costas block
  occupies symbols 0..6). The phantom tone takes the value of
  `re79[0]` whether that is a data tone or a Costas tone; this is
  intentional because the ramp-up before symbol 0 is, in the
  transmitter, at the Costas frequency.
- **Subtraction artifact at phantom boundary**. If the phantom-
  symbol amplitude does not exactly hit zero at the buffer edge
  before the ramp-up begins, residual discontinuity at the phantom
  start creates a wideband artifact. Use the same Gaussian-like
  ramp shape as the within-frame transitions to keep this clean.
- **Phase continuity**. The phantom symbol's phase should be
  back-propagated from the first real symbol's known phase at its
  start-of-symbol boundary, so that the phantom's trailing edge
  exactly matches the leading edge of symbol 0. SDRangel's
  implementation handles this implicitly because both the phantom
  and the real symbol share a single phase accumulator.

## Conflict with pancetta's existing mechanisms

Pancetta's spectral-subtraction code (whatever currently exists in
`pancetta-ft8/src/decoder/`) almost certainly only subtracts the 79
data + Costas symbols proper. Adding the tapered phantom-symbol
extension is **additive**: it leaves the within-frame subtraction
behavior unchanged when `subtract_edge_symbols = 0` (the default),
and adds two extra subtraction kernels per decoded frame when
enabled.

Interaction with hb-218 (capture-effect joint decode work, currently
plan-sized in pancetta's hypothesis bank): this mechanism is a
**partial alternative** to the joint-decode approach. Where joint
decode tries to decode multiple overlapping signals simultaneously,
tapered edge subtraction tries to reduce one signal's residue so
the other becomes decodable in a subsequent pass. The two are
**complementary** — both can be enabled, and the joint-decode
approach will benefit from a cleaner residual.

False-positive risk: subtracting phantom signal energy that isn't
really there can mask real weak signals. Specifically, if the
phantom-symbol tone choice is wrong (the actual transmitter ramped
up from silence with no carrier present), the subtraction injects
negative energy. In practice the FT8 transmitter ramp shape and
phase are well-defined and the conservative tone choice (boundary
symbol's tone) is correct.

## Estimated Rust port effort
- ~80-120 LOC inside the existing `subtract()` function.
- Re-uses pancetta's existing ramp-shape and per-symbol cosine
  modulator. No new infrastructure.
- 1 session: (S1) port the two phantom-symbol blocks behind a
  config flag, run hard-200 with the flag on vs off, measure
  capture-effect recall delta in the hb-218 measurement
  (companion-signal-within-±25Hz bucket).

## Implementation notes for the implementer thread

- Location: pancetta's existing spectral-subtraction function (the
  pancetta analog of SDRangel's `subtract()`). Wrap the new behavior
  in `if (params.subtract_edge_symbols)` blocks before and after the
  main per-symbol loop.
- Re-use the existing per-symbol cosine generator and per-symbol
  ramp generator. The only new code is the **range setup** for the
  phantom symbol (one half-ramp before symbol 0, one half-ramp after
  symbol N-1) and the **boundary-tone choice** (`re79[0]` and
  `re79[N-1]`).
- Default the new config flag to `0` (disabled). Land it as opt-in,
  evaluate against hard-200, then consider flipping default-on after
  the capture-effect bucket shows positive recall delta with no FP
  regression.
- Eval target: pancetta's hb-218 capture-effect joint-decode metric.
  Per hb-218 the headroom is 1058 truths blocked by a companion
  signal within ±25 Hz on the same WAV. Tapered edge subtraction
  targets specifically the temporal-overlap portion of that
  population. Expected delta: ~5-15% recovery of those 1058 truths
  if the temporal-overlap portion is 10-30% of capture-effect
  blocks.
- Pancetta's hardware-tier classifier should leave this mechanism
  alone — it costs ~2 extra symbol subtractions per decoded frame,
  trivial cost relative to one full LDPC decode. Default-on once
  validated for all three tiers.
- Watch for **phase mismatch on the boundary tone**. The phantom
  symbol must end (for pre-frame) or start (for post-frame) at
  exactly the phase of the first / last real symbol; otherwise the
  subtraction creates a wideband artifact that defeats the purpose.
  The cleanest expression is to run the phantom symbol's modulator
  off the same phase accumulator that the real-frame modulator
  uses, advancing it backwards (or forwards) by one symbol period.

## Cross-references
- `spec-wsjtr-dt-refinement-during-subtract.md` — pancetta has
  already specced one subtraction-stage refinement from wsjtr;
  tapered edge subtraction is a different (orthogonal) refinement.
- Pancetta hypothesis bank hb-218 (capture-effect joint decode) —
  related coverage target.
- pancetta hypothesis bank hb-100 (capture-effect boundary
  characterization) — established that the boundary is approximately
  ±25 Hz on equal-amplitude signals; tapered edge subtraction
  affects the **temporal** dimension of capture effect, not the
  frequency dimension.
