# Algorithm spec: JS8Call decoder survey

## Source attribution

- Origin: JS8Call (the original) and JS8Call-Improved (the active fork)
  - Original: https://github.com/js8call/js8call (was widefido on
    BitBucket; KN4CRD, Jordan Sherer)
  - Improved fork: https://github.com/JS8Call-improved/JS8Call-improved
    (org founded by Chris AC9KH; AC9KH and others)
- File paths visited (for traceability, NOT to be quoted):
  - Original: top-level C++ + Fortran tree; `lib/` carries Fortran kernels
    (`lib/decoder.f90`, `lib/js8/*.f90`, `lib/js8a_decode.f90`,
    `lib/js8b_decode.f90`, `lib/js8c_decode.f90`, `lib/js8e_decode.f90`,
    `lib/js8i_decode.f90`, plus matching `js8*_module.f90` modules)
  - JS8Call-Improved: `JS8_Mode/` (decoder kernel ported to C++) and
    `JS8_JSC/` (text compression dictionary). Notably no Fortran in
    the kernel — the project advertises that "use of Fortran has been
    eliminated".
- License: GPL-3.0 for both (verified via GitHub repo metadata and the
  `COPYING` file).
- Reader date: 2026-06-08

## Purpose of this survey

To document the JS8 protocol and decoder pipeline as it relates to
pancetta's FT8 decoder, identify which mechanisms could meaningfully
back-port to pancetta-ft8 (or to a future pancetta-js8 crate), and to
note where the JS8 decoder diverges from the WSJT-X / wsjtr / ft8mon
lineage that pancetta has already absorbed.

This document is the survey. Specific algorithm specs that warrant
their own deep dives are split out into separate files (see
"Recommended individual specs" at the end).

## High-level architecture

### Protocol facts (JS8 vs FT8)

JS8 is a sister mode to FT8 created by KN4CRD in 2018, evolved from
the FT8 protocol to support keyboard chat over weak signals. Key
identity vs FT8:

- FT8: 15-second slot, 79 symbols, 8-GFSK at 6.25 baud, 50 Hz wide,
  three 7×7 Costas sync arrays (start, middle, end), (174,87) LDPC,
  packed 77-bit payload, structured message templates only.
- JS8: same family — 8-GFSK at 12 ksps audio, free-text content via a
  JSC ("JS8 Static Code") dictionary, three Costas sync arrays but
  potentially with **a modified Costas pattern** (see below), and
  several **submodes** that trade speed for sensitivity (Normal /
  Fast / Turbo / Slow / Ultra-slow, also called "JS8 30 / JS8 40 / JS8
  10 / JS8 60 / JS8 5" by their nominal period in seconds).

The submode interface is exposed via a function table in JS8Call-
Improved's `JS8Submode.h` (a header that declares accessors but does
not enumerate the numeric values — they are produced at runtime; the
project mentions setting `QT_LOGGING_RULES=js8submode.js8=true` to dump
them to stderr). Cited period anchors from inline comments: SLOW is
30 s and the mode formerly called "Turbo" is 6 s.

### Two Costas array variants

`JS8.h` distinguishes ORIGINAL vs MODIFIED Costas array sets. The
ORIGINAL set is used by JS8 Normal mode and is inherited from FT8 (the
familiar (3,1,4,0,6,5,2) pattern). The MODIFIED set is used by all
non-Normal submodes; the actual sequences are tables in `JS8.cpp`.
Each variant is three 7-element arrays (start, middle, end of frame),
matching FT8's three-Costas structure.

**Implication for pancetta**: an FT8 decoder cannot decode non-Normal
JS8 transmissions without the modified Costas table loaded. Adding
JS8 support to pancetta-ft8 is a feasible delta — same demodulator
plumbing, different sync template per submode.

### Decoder pipeline shape

The original (Fortran) decoder uses the same overall shape as WSJT-X /
wsjtr / JTDX: front-end short-time FFT magnitude → Costas-templated
sync candidate search → time/frequency refinement → symbol soft
estimation → LDPC decode → optional multi-pass with subtract-and-re-
decode (SIC) → CRC check → message unpack. Per-submode `js8*_decode.f90`
files differ mainly in symbol count and stride; the core mechanics are
shared.

JS8Call-Improved has rewritten the kernel in C++ but kept the pipeline
shape. The major structural differences advertised:

- "The requirement for a separate decoder process and use of shared
  memory has been eliminated." (Upstream WSJT-X / JTDX run the
  decoder as a forked `jt9` / `ft8d` binary; JS8Call-Improved decodes
  in-process.)
- "Decode depth is now fixed at 2 in all cases." (Upstream WSJT-X
  exposes selectable ndepth = Fast / Normal / Deep; JS8Call-Improved
  collapses that.)
- Qt6 migration; spectrum/waterfall rewrite.
- Each FT8/JS8 submode is a separate `js8*_decode.f90` upstream, or a
  parameterized class in the C++ port.

### Text-side innovation: JSC dictionary

JS8 supports free-form text (which FT8 does not). It does so via a
dictionary-based compression scheme called JSC, implemented in
`JS8_JSC/JSC.cpp`, `JSC_list.cpp`, `JSC_map.cpp`, with a checker for
validating dictionary words. This is uncopyrightable mechanism but
the dictionary itself is a curated asset — re-using it in pancetta
would mean re-deriving the word list, not copying. Out of scope for
pancetta's FT8-only mission unless a JS8 crate is opened.

## What's actually different vs WSJT-X / wsjtr (the part pancetta cares about)

The JS8Call-Improved C++ kernel introduces **four mechanisms** in
`JS8_Mode/` that are genuinely interesting from a decoder-algorithm
standpoint and that pancetta has not catalogued from any other source:

1. **LDPC feedback refinement** (`ldpc_feedback.h`). An iterative LLR-
   refinement loop wrapped around the LDPC decoder. After each LDPC
   pass produces a candidate codeword, the LLRs feeding the next pass
   are boosted where the codeword agrees with the soft input and
   attenuated (or erased) where they disagree, gated by an erasure
   threshold. Tuning parameters (threshold, max iterations) are
   exposed via environment variables. Conceptually related to
   bit-flipping / belief-propagation refinement, but applied as a
   meta-loop *around* the LDPC decoder rather than inside it. See
   `spec-js8call-ldpc-feedback-refinement.md`.

2. **Soft combiner across repeats** (`soft_combiner.h`). A keyed LRU
   cache that recognises when the same candidate (mode, frequency bin,
   time bin, 32-bit LLR signature with Hamming-distance-≤4 fuzzy
   matching) has been received multiple times, and additively combines
   the LLR streams across receptions to drive deeper decodes from
   accumulated softness. Successful decodes evict the entry; failed
   entries age out. This is essentially soft-decision time diversity
   without the operator having to ask for it. See
   `spec-js8call-soft-combiner.md`.

3. **Whitening LLR normalizer** (`whitening_processor.h`). Per-tone
   (over all symbol times) and per-symbol (over non-winning tones)
   noise estimation, followed by divisive normalisation of the LLRs by
   the geometric mean of the two noise estimates, an optional erasure
   step for weak normalized LLRs, and a final variance-based
   standardisation. Acts on the 8×ND magnitude matrix that a JS8/FT8
   decoder already computes; output is a more comparable, more LDPC-
   friendly LLR stream. The "whitening" label refers to noise
   whitening of the LLR distribution, not spectral whitening of the
   audio. See `spec-js8call-llr-whitening.md`.

4. **Per-candidate frequency tracker** (`FrequencyTracker.h /.cpp`).
   A lightweight PLL/Kalman-style residual-frequency tracker that runs
   *inside the decode loop for each candidate*, not across a QSO. It
   takes the coarse sync's frequency estimate and refines it
   adaptively using pilot-tone (Costas-symbol) residuals as the symbol
   block is consumed, with a damping factor (alpha) and bounded
   step/error parameters. Output is a frequency-corrected complex
   stream and the tracked offset. See
   `spec-js8call-per-candidate-frequency-tracker.md`.

These four are the survey's load-bearing find. None of them duplicate
anything pancetta has already absorbed from wsjtr, ft8mon, or JTDX.

## Same as WSJT-X / wsjtr / FT8 family (NOT spec-worthy)

- The Costas sync detector structure (template correlation across
  three sync slots) — same mechanism, just with a swapped tone
  pattern table for non-Normal JS8 submodes.
- The (174,87) LDPC code, or the JS8 equivalent on per-submode
  parameters. The fast belief-propagation kernel is structurally
  identical to ft8_lib's; JS8Call-Improved just wraps it in C++.
- Multi-pass subtract-and-re-decode (SIC). Already extracted from
  ft8_lib / wsjtr origins for pancetta-ft8.
- Slot timing, sample-rate, FFT framing. Standard.
- The packjt / packjt77 message packing for structured messages.
  JS8 has its own packers for free-text via the JSC dictionary, but
  the structured-message paths share lineage with WSJT-X.

## Original JS8Call vs JS8Call-Improved (which to read for what)

- For **protocol facts** (Costas tables, submode parameters, frame
  structure): both repos are usable. The original Fortran source in
  `widefido / js8call` (mirrored at `js8call/js8call` and
  `atvfool/js8call`) is the canonical reference for the
  protocol-as-shipped through v2.3.1.
- For **the four reusable algorithmic mechanisms** (LDPC feedback,
  soft combiner, LLR whitening, per-candidate frequency tracker): the
  JS8Call-Improved C++ port is the only place these exist. They are
  not in the original Fortran kernel.
- The original BitBucket / `widefido` repo is in archived state; Jordan
  KN4CRD endorsed the move and joined the JS8Call-Improved team. The
  rename to "JS8Call" applies from v2.5.0 forward.

## Provenance and clean-room notes

- All four reusable mechanisms appear to be original JS8Call-Improved
  work, not back-ports from WSJT-X / JTDX. They have no obvious
  Fortran ancestor in the original JS8Call decoder line.
- Clean-room rule for pancetta: spec these in their own files (this
  reader has not opened the implementation `.cpp` bodies — only the
  headers — to minimise expression contamination), and have a
  separate implementer thread synthesise the Rust port from the spec
  alone. Source files have been **named** here for traceability but
  not quoted.

## Conflict with pancetta's existing mechanisms

- Pancetta already has SIC (subtract-and-re-decode), Costas sync, LDPC
  with belief-propagation, optional OSD, and per-pass variation (per
  `spec-wsjtr-per-pass-variation.md`).
- The four new mechanisms are **additive**, not replacements. They
  layer between existing pipeline stages:
  - LDPC feedback: wraps the LDPC stage.
  - Soft combiner: runs at candidate-emit time, before LDPC.
  - LLR whitening: runs at LLR-extraction time, between symbol-mag
    matrix and LDPC.
  - Per-candidate frequency tracker: runs at sync-refine time,
    between Costas candidate and symbol demod.
- All four respect the current pancetta hot-loop shape (`decoder.rs`
  per-candidate pipeline).

## Estimated Rust port effort

Per individual spec, but rough sizing:

| Mechanism                            | LOC est. | Sessions |
|--------------------------------------|---------:|---------:|
| LDPC feedback refinement             |    ~150  |    1–2   |
| Soft combiner across repeats         |    ~250  |      2   |
| LLR whitening                        |    ~180  |    1–2   |
| Per-candidate frequency tracker      |    ~120  |    1–2   |

Total survey-driven decoder uplift: ~700 LOC, ~6–8 implementer
sessions if all four ship. Each is independently shippable.

## Implementation notes for the implementer thread

- All four mechanisms are read-only at the data level: they consume
  existing intermediate state (LLRs, symbol magnitudes, candidate
  metadata) and produce a refined version of the same state. No
  changes to the message-bus, coordinator, or QSO state machine are
  required.
- The soft combiner is the only mechanism with persistent cross-slot
  state. It should live behind an `Arc<Mutex<…>>` (or a `parking_lot`
  RwLock) on the decoder context, with a TTL config knob.
- All four expose environment-variable knobs in the C++ original; in
  pancetta these become `pancetta-config` fields with sensible
  defaults that match the JS8Call-Improved defaults documented in
  their respective specs.
- Plumbing for the JS8 modified Costas table is a **separate**
  feature (a `JsMode` enum, per-mode sync template) and not required
  for porting any of the four mechanisms — they are mode-agnostic.

## Recommended individual specs

Created as part of this batch:

- `spec-js8call-ldpc-feedback-refinement.md`
- `spec-js8call-soft-combiner.md`
- `spec-js8call-llr-whitening.md`
- `spec-js8call-per-candidate-frequency-tracker.md`

NOT created (deferred):

- A full JS8 protocol spec (`spec-js8-protocol.md`) — only needed if
  pancetta decides to add JS8 mode support. The Costas table swap,
  per-submode symbol count, and JSC dictionary are all in scope when
  that decision is made.
