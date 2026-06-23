# Algorithm spec: Generic Gray decoding of soft-bit magnitude arrays (waveform-agnostic)

## Source attribution
- Origin: SDRangel libft8 (Edouard Griffiths, F4EXB), an
  independently-evolved fork of ft8mon (Robert Morris, AB1HL)
- File path: `ft8/ft8.h` (declaration of `un_gray_code_r_gen`) and
  `ft8/ft8.cpp` (definition); call sites in
  `plugins/channelrx/demodchirpchat/chirpchatdemoddecoderft.cpp`
  (ChirpChat / chirp-spread-spectrum demodulator that carries a
  174-bit FT8 LDPC payload over LoRa-style chirp symbols)
- License: GPL-2.0 (project), GPL-3.0 (libft8 derived from ft8mon)
- Reader date: 2026-06-08

## Why this is a clean delta vs ft8mon

ft8mon ships a single Gray-decoding helper, `un_gray_code_r`, that is
**hard-coded to the FT8 constellation**: it takes a 79×8 float array,
walks the rows, and re-orders the 8 columns through the fixed FT8 Gray
map `{0,1,3,2,6,4,5,7}` (or pancetta's equivalent). Its purpose is to
permute the 8 per-tone magnitudes/scores from "tone index order" to
"binary value order" so that bit-extraction loops can simply mask the
binary representation.

SDRangel introduces a **second, generalized** sibling:
`un_gray_code_r_gen`. This was not back-ported from ft8mon — `grep`
against `rtmrtmrtmrtm/ft8mon` returns zero hits for the `_gen`
function name family. It is genuinely SDRangel-side work, motivated by
needing the same Gray-decoding step for waveforms with a different
symbol bit-width (notably ChirpChat, where one chirp symbol can carry
4, 5, 6, 7, or more bits depending on spreading factor).

The cross-cutting insight (which is the algorithm idea, separable from
SDRangel's expression) is: **Gray decoding of a soft-bit magnitude
array is not waveform-specific.** The same one-line Gray-to-binary
formula works for any constellation of size `M = 2^nbSymbolBits`, so
the per-symbol vector of magnitudes can be permuted by a single
helper that infers `nbSymbolBits` from `log2(mags[0].size())` and uses
the closed-form `binary = gray ^ (gray >> 1)` style mapping rather
than a lookup table.

## Purpose

To produce a single waveform-agnostic Gray-to-binary permutation step
that can be shared across:
- the existing FT8 decoder path (`M = 8`, replaces the hard-coded
  lookup-table helper);
- the FT4 decoder path (`M = 4`, currently a duplicated hard-coded
  helper);
- any future / experimental FSK-like waveform carrying an LDPC payload
  on `M = 2^k` tones.

Concretely it removes the need to maintain multiple constellation-
specific Gray-decoding helpers — a small but real source of bugs
when the FT8, FT4, and ChirpChat paths drift apart over time.

## Algorithm description (PROSE ONLY)

### Inputs
- `mags` — a per-symbol-time vector of per-tone soft magnitudes.
  Outer length is the number of symbol time-positions (79 for FT8,
  105 for FT4, variable for ChirpChat). Inner length is exactly
  `M = 2^nbSymbolBits` — i.e. the number of constellation tones at
  that symbol position. Element `mags[t][i]` is the magnitude (or any
  scalar evidence score) the front end assigned to tone index `i`
  occurring at symbol time `t`.
- `nbSymbolBits` is **inferred** from `log2(mags[0].size())` rather
  than passed as a separate argument. (SDRangel does this explicitly
  via the inner-vector size; the implementer can pass it as an
  argument too — both are equivalent.)

### Output
- A new per-symbol vector of per-tone magnitudes of the same shape as
  `mags`, with the inner-vector entries **re-ordered from Gray index
  to binary index**. Element `out[t][b]` is the magnitude that
  corresponds to the symbol whose binary value is `b` — directly
  consumable by the bit-extraction loop downstream.

### Steps
1. Allocate `out` with the same outer length as `mags` and the same
   inner length `M = mags[0].size()`.
2. For each symbol time `t` in `0..mags.size()`:
   - For each binary value `b` in `0..M`:
     - Compute the Gray index `g` corresponding to binary value `b`
       using the standard reflected-binary Gray code formula
       `g = b XOR (b >> 1)`.
     - Copy `out[t][b] = mags[t][g]`.
3. Return `out`.

The single line `g = b XOR (b >> 1)` replaces the lookup table for any
`M = 2^k`. This is the reflected-binary Gray code definition; it is
not a copyrightable expression, it is a textbook fact (Frank Gray,
1947) that the FT8 protocol and every multi-FSK system in radio uses.

### Why no Costas handling here
Costas-tone symbol positions are NOT excluded by this helper. The
helper is purely a per-row permutation. The caller is expected to
already have stripped or to subsequently ignore Costas symbol indices.
This separation of concerns is what lets the same helper serve FT8
(three Costas blocks of 7 each), FT4 (four Costas blocks), and
ChirpChat (no Costas at all).

### Numerical constants (facts, not expression)
- For FT8: `M = 8`, `nbSymbolBits = 3`, outer length = `79`.
- For FT4: `M = 4`, `nbSymbolBits = 2`, outer length = `105`.
- For ChirpChat-on-FT8-payload: typical `M = 64..128` for spreading
  factors 6..7; the same helper still applies.
- The reflected-binary Gray formula `g = b XOR (b >> 1)` is the
  standard convention used by FT8, FT4, JT9, and the LoRa physical
  layer. Confirm pancetta's existing FT8 Gray map matches before
  flipping over.

### Edge cases
- **Inner-vector size not a power of two.** Reject at the caller —
  the formula assumes `M = 2^k`. SDRangel's call sites guarantee this
  by construction (the front end builds the per-tone magnitude vector
  exactly `M`-wide).
- **Empty outer vector.** Trivially returns an empty outer vector.
  Useful for "Costas-only" symbol-position slices where there are no
  data bits.
- **Convention mismatch.** Different protocols / different references
  (Wikipedia vs WSJT-X notebooks vs Atmel datasheets) sometimes use a
  different Gray code convention. The formula above is one common
  one. The spec-level guarantee is "round-trip with the encoder":
  whatever the encoder used to permute binary-to-tone, this helper
  must invert. Validate by sending each binary value `b` through
  encoder Gray-map then through this helper and asserting recovery.

## Conflict with pancetta's existing mechanisms

Pancetta has its own FT8 Gray map in `pancetta-ft8`. The map is
hard-coded as a const table, equivalent to SDRangel's
`un_gray_code_r`. There is **no functional gap** for the pure FT8
case — the new mechanism brings zero decoder accuracy benefit on FT8
alone. The benefit only materializes when pancetta either:
- adds an FT4 decoder (then a generalized helper avoids duplicating
  the table for `M = 4`), or
- adds an experimental FSK-like waveform variant (ChirpChat is not on
  any roadmap I can see in CLAUDE.md), or
- wants a single permutation helper validated by exhaustive
  round-trip tests for any `M = 2^k`.

For now this spec is **infrastructure-only** in pancetta context. I am
recording it so that the next time someone reaches for "Gray decoding
from magnitudes" they don't re-derive it.

## Estimated Rust port effort
- ~30 LOC for the helper itself plus ~30 LOC of round-trip property
  tests.
- 0.5 sessions if pancetta already has a Gray map elsewhere — this is
  a re-expression with `(b ^ (b >> 1))` in place of a lookup.

## Implementation notes for the implementer thread
- Place the helper in `pancetta-ft8/src/decoder/gray.rs` (or wherever
  the existing Gray map lives). Keep the existing FT8 lookup table as
  the validation reference: a property test asserts the generalized
  formula and the lookup table agree for `M = 8`.
- Signature suggestion: `fn un_gray_code_rows<T: Copy>(mags: &[Vec<T>])
  -> Vec<Vec<T>>` with the inner-vector size inferred at runtime.
  Generic over the scalar type so the same helper handles `f32`
  magnitudes, `f32` SNRs, and `Complex<f32>` (used by `c_soft_decode`
  and the pair / triple paths).
- This helper does NOT do bit extraction. Bit extraction (taking
  `out[t][·]` and computing per-bit max-for-zero / max-for-one) is the
  next pipeline stage and is shared with the spec
  `spec-sdrangel-generalized-soft-decode.md`.
- Cost: 79×8 row permutations per call for FT8. Negligible — well
  under 1 microsecond. The reason to use the formula over the lookup
  is not speed, it is generality.

## Cross-references
- Companion spec `spec-sdrangel-generalized-soft-decode.md` — the
  full `soft_decode_mags` pipeline that this helper feeds.
- `spec-ft8mon-soft-decode-pairs.md` — uses the existing
  non-generalized Gray decoder.
- `spec-ft8mon-three-soft-decoder-ensemble.md` — single / pair /
  triple variants, all of which use the existing FT8-specific Gray
  decoder; none currently exploit the generalization.
