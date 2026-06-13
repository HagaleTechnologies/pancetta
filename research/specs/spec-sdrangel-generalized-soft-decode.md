# Algorithm spec: Generalized magnitude-only soft demodulator for arbitrary M-ary FSK carrying a 174-bit LDPC payload

## Source attribution
- Origin: SDRangel libft8 (Edouard Griffiths, F4EXB), an
  independently-evolved fork of ft8mon (Robert Morris, AB1HL)
- File path: `ft8/ft8.h` and `ft8/ft8.cpp` for:
  - `soft_decode_mags` (the generalized demodulator itself)
  - `convert_to_snr_gen` (the generalized per-symbol noise normalizer)
  - `make_stats_gen` (the generalized empirical-distribution builder)
  - `un_gray_code_r_gen` (see companion spec)
- Call site: `plugins/channelrx/demodchirpchat/chirpchatdemoddecoderft.cpp`
  in the function `decodeWithShift`, which carries an FT8 174-bit
  LDPC codeword over LoRa-style chirp spread-spectrum symbols.
- License: GPL-2.0 (project), GPL-3.0 (libft8 derived from ft8mon)
- Reader date: 2026-06-08

## Why this is a clean delta vs ft8mon

ft8mon's single-symbol magnitude-only soft demodulator (`soft_decode`,
`use_ones=1`) is **hard-coded to FT8's 79-symbol-by-8-tone structure**.
The Costas-block positions, the 174-bit LDPC layout, and the
3-bit-per-symbol Gray map are all woven into the function body.

SDRangel introduces a **generalized parallel sibling**, `soft_decode_mags`,
that takes the same magnitude-only soft-demod idea and parameterizes
it on `nbSymbolBits`. With `nbSymbolBits = 3` it is functionally the
FT8 soft demod; with `nbSymbolBits = k` it is the same algorithm on a
`2^k`-tone constellation. Grep against `rtmrtmrtmrtm/ft8mon` for
`soft_decode_mags`, `convert_to_snr_gen`, and `make_stats_gen`
returns zero hits — these are SDRangel-side additions.

The generalization is **not** an algorithm change vs the ft8mon
single-symbol demod for the pure FT8 case. The novelty is purely
structural: the demodulator is now waveform-agnostic, lifting the
hard-coded 8-tone / 79-symbol / FT8-Gray-map assumptions into runtime
parameters.

For pancetta this matters in two ways:
1. As **decoder design discipline**: a generalized magnitude-only
   demod is a cleaner internal API than the entangled FT8-specific
   `soft_decode`. Pancetta's existing soft demod is closer to the
   entangled version. Refactoring along these lines would make the
   pair / triple specs simpler to add later.
2. As a path to **carrying FT8 payloads over alternative physical
   layers** (e.g. a wider-bandwidth Costas-less FSK on contested
   bands). Not on any current roadmap, but worth recording.

The 2024-era mainline difference, in F4EXB's words from the header,
is "generalized to any number of symbol bits ... used by FT-chirp
modulation scheme." That sentence is the most explicit acknowledgement
in the source.

## Purpose

To produce a 174-element soft-bit (log-likelihood-ratio) vector from
a per-symbol-time array of per-tone magnitudes of arbitrary
constellation size `M = 2^nbSymbolBits`. The output is the standard
174-bit LDPC payload, consumable by the existing LDPC + CRC stage
without modification.

The key advantage over the FT8-specific `soft_decode`:
- Same code path for FT8 (M=8), FT4 (M=4), and any future
  M-ary FSK variant.
- No Costas symbols in the input — the caller is expected to have
  excluded those before calling.
- No 79-symbol assumption — the caller provides exactly enough symbol
  rows that `n_symbols × nbSymbolBits = 174`. For FT8 that is 58
  data rows; for FT4 that is 87 data rows; for ChirpChat-on-FT8 the
  shape varies with spreading factor and message bits.

## Algorithm description (PROSE ONLY)

### Inputs
- `mags` — outer-vector of per-symbol-time vectors of per-tone
  magnitudes. Outer length is the number of data-symbol positions
  (Costas-stripped). Inner length is exactly `M = 2^nbSymbolBits`.
- `nbSymbolBits` — integer ≥ 1; how many LDPC bits each symbol
  position carries. Total bits emitted = `outer_len × nbSymbolBits`,
  must equal 174 for the FT8 LDPC payload.
- A reference to the shared `FT8Params` object that controls the
  Bayes conversion (specifically `problt_how_noise`, `problt_how_sig`,
  `log_tail`, `log_rate`, and `bayes_how`).

### Output
- A 174-element `f32` LLR array. Sign convention: positive favors
  bit = 0, negative favors bit = 1.

### Steps

1. **Normalize per-symbol-time magnitudes** via `convert_to_snr_gen`
   (described below). Result: a new outer × inner array of the same
   shape with each row scaled so that its noise floor is approximately
   unit-magnitude, washing out per-symbol-time level differences.

2. **Gray-decode the per-tone permutation** via `un_gray_code_r_gen`
   (see companion spec `spec-sdrangel-gray-decode-from-magnitudes.md`).
   Result: a new outer × inner array where `out[t][b]` is the
   magnitude that corresponds to binary value `b` at symbol time `t`,
   rather than to Gray tone index `g`.

3. **Build slot-local empirical distributions** via `make_stats_gen`:
   - `all` — sorted list of every magnitude in the Gray-decoded
     `outer × inner` array. Represents the noise / null reference.
   - `bests` — sorted list of one "best per row" magnitude per symbol
     time. For each row, the bracket choice of which magnitude to
     promote is the row maximum (best tone hypothesis at that symbol
     time). Represents the "signal-present" reference.

4. **Iterate over the 174 bit positions** in row-major order:
   For each row `t` in `0..outer_len`, for each bit `b_in_row` in
   `0..nbSymbolBits`:
   - Compute `bit_index = t × nbSymbolBits + b_in_row`.
   - **Per-bit max evidence accumulator**: walk all `M` tones in row
     `t`. For each tone index `i` (after Gray decoding, so `i` is the
     binary value), check whether bit `b_in_row` of `i` is 0 or 1.
     Keep two scalars per bit:
     - `m0` = the largest magnitude over all `i` whose bit
       `b_in_row` is 0.
     - `m1` = the largest magnitude over all `i` whose bit
       `b_in_row` is 1.
   - **Bayes conversion**: use the `bests` and `all` distributions to
     map each magnitude to two cumulative probabilities — the
     probability that observation ≥ `m0` is signal (from `bests`),
     and the probability that observation ≥ `m0` is noise (from
     `all`). Compute `p0 = P(≥m0 | sig)`, `q0 = P(≥m0 | noise)`,
     and `p1`, `q1` symmetrically for `m1`. The LLR is
     `log( (p0 × q1) / (p1 × q0) )`. The sign convention is positive
     when `m0 > m1` (bit=0 favored).

5. **Output the 174-LLR vector** in `b_in_row`-then-`t` order
   matching the LDPC convention.

### `convert_to_snr_gen` — the generalized noise normalizer

- Signature: takes the shared `FT8Params`, `nbSymbolBits`, and the
  raw magnitudes vector-of-vectors; returns a same-shape normalized
  vector-of-vectors.
- Mechanism (paraphrased from the non-generalized `convert_to_snr`):
  for each tone-column independently, compute a windowed-median noise
  estimate across the time axis using a window length controlled by
  `snr_win` (default 7 symbols) and a window-shape control
  `win_type`, then divide each magnitude by that local noise estimate.
- The generalization vs the FT8-specific version is that the
  per-tone-column loop is bounded by `M = 2^nbSymbolBits` rather than
  by the constant 8, and the per-symbol-time loop is bounded by
  `mags.size()` rather than by 79.
- Edge handling at the time-axis boundaries uses the same shoulder /
  extra parameters as the FT8 version (`shoulder200`, `tminus`,
  `tplus`); pancetta should reuse whatever shoulder mechanism the
  existing single-symbol path already has.

### `make_stats_gen` — the generalized distribution builder

- Builds the `all` and `bests` empirical distributions from the
  Gray-decoded normalized magnitudes.
- The "no Costas used" comment in SDRangel's source is the key
  delta vs `make_stats`: this version does **not** carve out the
  Costas-block positions, because Costas tones are assumed already
  absent from the input.
- Distribution storage: `Vec<f32>` sorted once, queried by
  `partition_point()` — same approach as
  `spec-ft8mon-soft-decode-pairs.md`.

### Numerical constants (facts, not expression)
- `nbSymbolBits` for FT8: `3`. For FT4: `2`. For ChirpChat:
  application-dependent (5..7 typical).
- `M = 2^nbSymbolBits`. Per-row magnitude vector length.
- `snr_win = 7` symbols (FT8Params default; carries to FT4 path).
- `bayes_how = 1` selects the Bayes posterior variant used by all
  three soft demodulators (`c_soft_decode`, `soft_decode_pairs`,
  `soft_decode_triples`, and now `soft_decode_mags`).
- `problt_how_noise = 0`, `problt_how_sig = 0` — defaults for the
  cumulative-probability lookups.
- `log_tail = 0.1`, `log_rate = 8.0` — defaults for the
  tail-extrapolation cap on the Bayes lookups (preventing
  `log(0)` blow-up).

### Edge cases
- **Inner-vector size not exactly `2^nbSymbolBits`**. Reject at the
  caller. The Gray formula assumes power-of-two width.
- **Total bit count != 174**. Reject at the caller. The LDPC stage
  is rigid about the input length.
- **Empty `bests` distribution at first bit conversion**. Same
  treatment as `soft_decode_pairs`: clamp `p` to a small epsilon
  before taking `log`. SDRangel's `problt` does this implicitly via
  the `log_tail` / `log_rate` defaults.
- **All-zero row** (degenerate FFT input). The per-row max becomes
  arbitrary; the resulting LLR for that row's bits will be near zero
  (no evidence either way). LDPC will treat those bits as erasures,
  which is the correct behavior.
- **Costas-block contamination**. If the caller forgets to strip
  Costas positions, those rows will look "abnormally bright on one
  specific tone" and will skew the `bests` distribution toward
  optimism. Validate caller-side that Costas positions are absent.

## Conflict with pancetta's existing mechanisms

Pancetta's current soft demod is the **magnitude-only single-symbol**
path with the 8-tone, 79-symbol, FT8-Gray-map assumptions baked in.
Adopting `soft_decode_mags` would be a **refactor with no FT8
accuracy benefit**: bit-for-bit identical LLR output on a pure FT8
input.

The two valid reasons to do the refactor:
1. **Decoder modularity**: enable the pair / triple / phase-coherent
   variants from `spec-ft8mon-three-soft-decoder-ensemble.md` to all
   share the same `convert_to_snr` and `un_gray_code_r` plumbing
   regardless of which constellation size they target.
2. **Future-proofing**: if pancetta ever adds an FT4 decoder, the
   generalized path is the natural home.

If pancetta's decoder is not on the path to either of those, this
spec is **infrastructure-only**. The mechanism is real and clean;
the FT8-specific decoder accuracy delta is zero. I am recording it
so it is available, not arguing for adopting it.

## Estimated Rust port effort
- ~200 LOC for `soft_decode_mags` proper.
- ~80 LOC for `convert_to_snr_gen` (re-expression of the existing
  per-tone windowed-median normalizer with the loop bounds lifted
  to runtime).
- ~50 LOC for `make_stats_gen` (essentially the same as the
  pair-spec `Stats` machinery).
- The `un_gray_code_r_gen` companion is ~30 LOC (separate spec).
- 1 session if the existing FT8 single-symbol demod is the only
  consumer (essentially a clean-up refactor); 2 sessions if also
  wiring the pair / triple variants from
  `spec-ft8mon-three-soft-decoder-ensemble.md` through the same
  plumbing.

## Implementation notes for the implementer thread

- The right Rust signature mirrors the SDRangel signature:
  `fn soft_decode_mags<const NB: usize>(params: &Ft8Params,
  mags: &[[f32; M]]) -> [f32; 174]` where `M = 2^NB`, or with NB as
  a runtime parameter if you prefer dynamic dispatch over generics.
  Generics let the compiler unroll the per-tone loop and produce
  identical codegen to the hand-written FT8-specific version.
- Reuse the `Stats` machinery from
  `spec-ft8mon-soft-decode-pairs.md`. Do not introduce a parallel
  empirical-distribution helper.
- Reuse the per-column windowed-median noise estimator from
  pancetta's existing `convert_to_snr`. Lift the column loop bound
  to a generic / parameter.
- **Validation strategy**: feed pancetta's existing FT8 hard-200
  corpus through both `soft_decode` (current) and `soft_decode_mags`
  with `M=8`. The LLR vectors should match to within float
  rounding. The decode counts should match exactly.
- **Do NOT change LDPC + CRC**. This is purely a soft-demod
  refactor.
- Watch the `problt_how_*` defaults — SDRangel's defaults
  (`= 0`, `= 0`) are not the same as ft8mon's. The Bayes lookup
  details determine whether pancetta gets bit-identical output on
  the validation. Pin both values to match whatever the existing
  pancetta demod uses; do not import SDRangel's defaults blindly.
- This refactor is **risky on the FP side** if any of the lookup
  conventions silently differ. Land it behind a feature flag and
  run hard-200 + a 100-WAV noise corpus before flipping the default.

## Cross-references
- `spec-sdrangel-gray-decode-from-magnitudes.md` — companion Gray
  decoder.
- `spec-ft8mon-soft-decode-pairs.md` — pair variant, currently
  uses the FT8-specific `un_gray_code_r` and `Stats` machinery.
- `spec-ft8mon-three-soft-decoder-ensemble.md` — the ensemble
  driver that currently chains `c_soft_decode`,
  `soft_decode_pairs`, `soft_decode_triples`. The generalization
  in this spec does not affect the ensemble structure — it only
  lifts the constellation assumption out of each demodulator.
- `spec-ft8mon-snr-windowed-blackman.md` (if it exists in
  pancetta's spec folder; if not, `convert_to_snr` is documented
  in-line in the pair spec) — the per-column noise normalizer
  this spec generalizes.
