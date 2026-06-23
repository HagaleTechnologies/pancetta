# Algorithm spec: WSJT-X mainline ft8b — inner decoder loop

## Source attribution

- Origin: WSJT-X mainline (K1JT et al., upstream)
- Repository: https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/
- Mirror: https://github.com/saitohirga/WSJT-X
- File (traceability only; NOT quoted): `lib/ft8/ft8b.f90` (~500 LOC)
- Companion: `lib/ft8/sync8d.f90`, `lib/ft8/ft8_downsample.f90`,
  `lib/ft8/subtractft8.f90`, `lib/ft8/decode174_91.f90`,
  `lib/ft8/osd174_91.f90`
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

`ft8b` is the *inner* decoder. For a single candidate emitted by `sync8`, it
performs:

1. Frequency-shift the candidate to baseband and downsample to 200 Hz
   (32 samples per symbol).
2. Refine the time and frequency estimates with a fine sync search.
3. Compute per-symbol complex amplitudes and four parallel bit metric
   variants.
4. Run a multi-pass decode loop: 4 "regular" passes (varying soft-demod
   strategy) + up to 4 "AP" (a priori) passes that inject known-callsign
   constraints into specific message-bit positions.
5. On a valid codeword: unpack the 77 message bits, validate the CRC-14,
   reject quirky message types, compute SNR, and optionally subtract the
   decoded waveform from the audio buffer for downstream passes.

This is the most complex single file in WSJT-X FT8 and the most
implementation-detail-dense. Wsjtr translates this into Rust across
several files; reading the original catches several paraphrasing artifacts.

## Inputs (caller arguments)

The Fortran signature is long. The interesting inputs (semantically):

- `dd0` — full 15-second audio buffer (180,000 samples at 12 kHz). May
  be mutated by signal subtraction if `lsubtract = .true.`.
- `newdat` — `.true.` if `dd0` content changed since last call (so the
  downsample-prep long FFT must be redone).
- `nQSOProgress` — operator's QSO-state hint, 0..5:
  - 0 = "I'm calling CQ"
  - 1 = "Tx1: sent grid"
  - 2 = "Tx2: sent report"
  - 3 = "Tx3: sent RRR"
  - 4 = "Tx4: sent 73"
  - 5 = "Tx5: sent RR73"
  - Drives the AP-pass schedule (`nappasses`, `naptypes` tables).
- `nfqso`, `nftx` — operator QSO and TX frequencies in Hz.
- `ndepth` — 1, 2, or 3 (BP-only / BP+OSD uncoupled / BP+OSD coupled).
- `nzhsym` — count of half-symbol spectrogram columns available so far.
  Discriminates "early" (47) vs "final" (50) passes.
- `lapon` — global "AP enabled" flag.
- `lapcqonly` — restrict AP to CQ-pattern only.
- `napwid` — AP frequency window in Hz: AP types ≥3 (MyCall+DxCall+…)
  only run for candidates within ±`napwid` of `nfqso` or `nftx`. Typical
  values: 50-150 Hz.
- `lsubtract` — subtract decoded signal from `dd0` on success.
- `nagain` — operator re-decode-this-signal request; uses a different
  SNR formula.
- `ncontest` — contest mode integer:
  - 0 = NONE, 1 = NA_VHF, 2 = EU_VHF, 3 = FIELD DAY, 4 = RTTY,
  - 5 = WW_DIGI, 6 = FOX, 7 = HOUND, 8 = ARRL_DIGI.
- `iaptype` — output: which AP pass actually succeeded, 0 = none.
- `mycall12`, `hiscall12` — operator's callsigns for AP.
- `f1`, `xdt` — candidate frequency / dt; **mutated in place** with
  refined values.
- `apsym(58)` — pre-packed AP symbol bits for MyCall and HisCall.
- `aph10(10)` — Hound-mode 10-bit Fox-call hash AP.

## Outputs

- `nharderrors` — number of hard-decision bits that disagree with the
  final codeword; `-1` if no decode.
- `dmin` — weighted (soft) distance metric for the decoded codeword.
- `nbadcrc` — `0` if CRC validates and message is "non-quirky"; `1`
  otherwise.
- `msg37` — the decoded 37-character message, or 37 spaces.
- `xsnr` — estimated SNR in dB.
- `itone(79)` — the 79 tone indices for the decoded message (used for
  subtraction and external display).

## Numerical constants

- `NDOWN = 60` — downsample factor (12000 / 60 = 200 Hz baseband).
- `NP2 = 2812` — length of the downsampled complex buffer.
- 200 Hz / 6.25 Hz-per-symbol = 32 samples per symbol after downsample.
- `max_iterations = 30` — BP iteration cap (passed to `decode174_91`).
- Coarse dt search: `±10` baseband samples around `i0 = nint((xdt+0.5)*fs2)`
  (i.e., ±50 ms; ±¼ symbol).
- Fine frequency search: `±5` steps of `0.5 Hz` = ±2.5 Hz total.
- Second coarse dt search (after frequency refinement): `±4` baseband
  samples (±20 ms; ±⅛ symbol). Output dt is the argmax of a 9-element
  array `ss(1:9)`.
- Sync quality threshold (early bail): `nsync <= 6` out of max 21 →
  reject candidate immediately. ("nsync" is hard sync sum: of the 21
  total Costas tones across 3 arrays, how many had the right tone at
  the right time as the spectral maximum.)
- Late-stage SNR bail: if `nsync <= 10` AND final `xsnr < -24 dB`,
  reject as likely false decode.
- LLR scale factor: `scalefac = 2.83`. Applied to all four bmet variants
  after their per-variant normalization to unit-variance.
- AP magnitude: `apmag = max(|llra|) * 1.01`. Locked AP bits get
  `±apmag` as their LLR — they must be just over the loudest soft bit
  so they're treated as ~certain by the BP decoder.
- Soft-symbol Gray map: `(0, 1, 3, 2, 5, 6, 4, 7)`. Inverts the
  FT8 8-FSK Gray code at the tone-to-bit-triplet boundary.

## Algorithm description (prose only)

### Step 1: downsample to baseband

`ft8_downsample` (helper) does this in the frequency domain: an FFT of
the full 180,000-sample buffer (`NFFT1 = 192000`), then a windowed
extraction of bins corresponding to `[f1 - 1.5*baud, f1 + 8.5*baud]`
(i.e., a 10-symbol-wide band centered on the candidate with 1.5 baud
guard below and 8.5 baud guard above — asymmetric on purpose to capture
the 8-FSK tone spread). The extracted band is tapered with a 101-point
Hann taper on each end, frequency-shifted to baseband via `cshift`, and
inverse-FFT'd at `NFFT2 = 3200` samples to produce the 200 Hz baseband
complex array `cd0(0:3199)`. Normalization: `1 / sqrt(NFFT1 * NFFT2)`.

The asymmetric guard band (1.5 below, 8.5 above) reflects that FT8 puts
its 8 tones *above* the carrier — bin 0 is the lowest tone, bin 7 the
highest. There's no signal energy below the lowest tone (only noise).

### Step 2: coarse dt refinement at the candidate

Walk `idt` over `±10` baseband samples around the candidate's `xdt`. For
each `idt`, call `sync8d` (helper) to compute a *fine-grained* Costas
sync power at that integer-sample offset, without frequency twiddling
(`itwk = 0`). Argmax → `ibest`.

`sync8d` is essentially a matched-filter version of the symbol-spectra
sync from `sync8`, but at 32 samples per symbol resolution and using
complex correlation rather than power spectra. It correlates the
baseband `cd0` against precomputed 32-sample complex Costas waveforms
for each of the 7 tones, at the time positions of all 3 Costas arrays
(symbols 0, 36, 72 — in baseband-sample units, `0, 36*32, 72*32`).
Sums power over all 21 (tone × array) correlations.

### Step 3: fine frequency refinement

For each `delf` in `(-2.5, -2.0, -1.5, …, +2.5)` Hz:

- Build a 32-sample complex twiddle vector `ctwk(i) = exp(j*2π*delf*dt2*i)`
  where `dt2 = 1/200`.
- Call `sync8d` with `itwk = 1` and the twiddle. This multiplies the
  reference Costas waveforms by `ctwk` before correlating — equivalent
  to checking "what if the candidate frequency is off by `delf`?"

Argmax → `delfbest`. Apply a *global* frequency twiddle to `cd0` (via
`twkfreq1`) of `-delfbest` so that the candidate is exactly on
baseband, and update `f1 += delfbest`.

Then re-run the downsample with the updated `f1` and (`newdat = .false.`,
so the long FFT isn't redone — only the bin extraction). This gives a
clean baseband `cd0` aligned to the refined frequency.

### Step 4: secondary fine dt refinement

After frequency refinement, sweep dt again: `idt` over `±4` baseband
samples around `ibest`, computing `sync8d(cd0, ibest+idt, ctwk, 0, sync)`.
Store the 9 sync values in `ss(1:9)`, take argmax → updated `ibest`,
update `xdt = (ibest - 1) * dt2`.

### Step 5: per-symbol DFTs

For each of the 79 channel symbols, take a 32-point FFT of the
corresponding baseband window `cd0(i1:i1+31)`, store the first 8 bins
in `cs(0:7, k)` and their magnitudes in `s8(0:7, k)`. These are the
per-symbol per-tone complex amplitudes and magnitudes used for soft
demodulation.

### Step 6: hard sync quality check

For each of the 21 Costas tones (7 per array × 3 arrays at symbols
0-6, 36-42, 72-78), check whether the *argmax* tone of `s8` matches
the expected Costas tone. Count matches: `nsync = is1 + is2 + is3`,
max 21. If `nsync <= 6`, bail out immediately (set `nbadcrc = 1`,
return). This is the hard pre-decode reject.

### Step 7: four parallel bit-metric variants

Compute four parallel sets of 174 bit metrics, then convert each to
LLRs. The four variants differ in the "neighbor-symbol context" used:

**Variant A (`bmeta`, `nsym = 1`).** Treat each symbol independently.
For each of the 58 data symbols (positions 7..42 and 43..78), iterate
over the 8 possible tones. For each output bit position `ib` in `[0, 2]`:
- `bm = max{ s_tone for one(tone, ibmax-ib)=true } - max{ s_tone for one(tone, ibmax-ib)=false }`
  where `one[tone, bit]` is a precomputed table: bit `bit` of `graymap[tone]`.
  `ibmax = 2` for nsym=1, so 3 output bits per symbol → 174 total bits.
- This is the standard 8-FSK soft-demod max-vs-max LLR proxy.

**Variant B (`bmetb`, `nsym = 2`).** Pair adjacent symbols. For each
2-symbol window, iterate over `8*8 = 64` possible tone-pairs. The
"signal" is `s2 = abs(cs(graymap(i2), ks) + cs(graymap(i3), ks+1))` —
the *coherent* (complex-amplitude) sum of two adjacent tones. Then
the same max-vs-max LLR computation, but now over 64 candidates and
`ibmax = 5` (6 output bits per pair).

**Variant C (`bmetc`, `nsym = 3`).** Triples of symbols. `8*8*8 = 512`
candidates, coherent sum of three adjacent complex amplitudes,
`ibmax = 8` (9 output bits per triple).

**Variant D (`bmetd`, "bit-by-bit normalized").** Like variant A, but
the LLR is divided by `max(max_one, max_zero)` per bit before
normalization. This is a per-bit "confidence ratio" rather than a raw
difference. The original comment: "if den=0, erase it" → `cm = 0`,
neutral LLR.

After all four are filled, each is normalized via `normalizebmet`:
subtract mean / divide by stddev (with a fallback if stddev is zero).
Then scaled by `scalefac = 2.83` → LLRs.

The four LLR sets `(llra, llrb, llrc, llrd)` correspond to four
different "depth-of-context" soft demods. They feed four separate
regular decode passes.

### Step 8: build the AP-pass schedule

The table `nappasses(nQSOProgress)` defines how many AP passes to add
after the 4 regular passes:

- `nQSOProgress = 0` (calling CQ): 2 AP passes
- `nQSOProgress = 1`: 2
- `nQSOProgress = 2`: 2
- `nQSOProgress = 3` (sent RRR): 4
- `nQSOProgress = 4` (sent 73): 4
- `nQSOProgress = 5` (sent RR73): 3

The table `naptypes(nQSOProgress, pass_index)` selects which "AP type"
each AP pass uses:

- AP type 1: CQ pattern (29 callsign bits + 3 grid bits = 32 AP bits)
- AP type 2: MyCall + ??? + ??? (29+3 = 32)
- AP type 3: MyCall + DxCall + ??? (58+3 = 61)
- AP type 4: MyCall + DxCall + RRR (77 — full payload)
- AP type 5: MyCall + DxCall + 73 (77)
- AP type 6: MyCall + DxCall + RR73 (77)

Mapping:
- `nQSOProgress = 0`: AP types {1, 2}
- `nQSOProgress = 1`: {2, 3}
- `nQSOProgress = 2`: {2, 3}
- `nQSOProgress = 3`: {3, 4, 5, 6}
- `nQSOProgress = 4`: {3, 4, 5, 6}
- `nQSOProgress = 5`: {3, 1, 2}

Contest-specific overrides exist (Hound mode forces AP; Fox skips AP).

If `nzhsym < 50` (early pass before full audio), force `npasses = 4`
(skip AP entirely — not enough data yet).

### Step 9: the main decode loop

For `ipass = 1` to `npasses`:

- Passes 1-4 use `llra, llrb, llrc, llrd` respectively, with
  `apmask = 0` (no AP bits locked).
- Passes 5+ use `llra` again, but with AP bits locked per the
  iaptype-specific apmask + LLR injection.

**AP guards (cycle the pass if any fails):**

- AP types ≥3 only run within `±napwid` of nfqso or nftx (except
  contest=7 Hound).
- Contest=6 Fox: no AP at all.
- Contest=7 Hound: AP only for signals below 950 Hz.
- AP types ≥2 require a usable MyCall (apsym(1) ≤ 1).
- AP types ≥3 require a usable DxCall (apsym(30) ≤ 1).

**AP type 1 (CQ pattern):** lock bits 1..29 to the contest-specific CQ
codeword (mcq / mcqru / mcqfd / mcqtest / mcqww — different 29-bit
patterns for "CQ", "CQ RU", "CQ FD", "CQ TEST", "CQ WW"), and lock the
i3/n3 bits 75..77 to `(-1, -1, +1)` (i.e., the FT8 message-type field
for "standard message"). The choice of mcq* depends on `ncontest`.

**AP type 2 (MyCall, ???, ???):** lock bits 1..29 to MyCall's
pre-packed symbols, plus i3/n3 to (-1,-1,+1). Contest-specific variants
adjust the bit ranges (e.g., NA_VHF uses bits 1..28 + a state/province
field at 72..74).

**AP type 3 (MyCall, DxCall, ???):** lock bits 1..58 (MyCall + DxCall)
+ i3/n3.

**AP types 4/5/6 (full message):** lock all 77 bits to MyCall + DxCall
+ {RRR | 73 | RR73}. These are the most aggressive AP types and only
help when the operator already knows the partner.

**Hound-specific AP (contest=7):** lock the 10-bit Fox-call hash via
`aph10`. This is for the FT8 contest "fox/hound" protocol where the Fox
station's call is hashed to fit in a 10-bit field.

### Step 10: invoke the LDPC/OSD decoder

```
maxosd:
  ndepth = 1 → maxosd = -1 (BP only)
  ndepth = 2 → maxosd = 0  (BP + 1 OSD with channel LLRs)
  ndepth = 3 + signal near nfqso/nftx (or Hound) → maxosd = 2 (BP + 2 OSDs with saved zsum)
  ndepth = 3 otherwise → maxosd = 0
norder = 2 (OSD order, but see osd174 spec for ndeep mapping)
Keff = 91 (use all 14 CRC bits cascaded with LDPC)
```

Call `decode174_91(llrz, Keff, maxosd, norder, apmask, …)` which runs BP
then falls back to OSD if BP fails.

### Step 11: post-decode validation

If `nharderrors < 0` or `> 36` → reject (decode failure or too many
errors flipped).

If the codeword is all-zero (`count(cw==0) == 174`) → reject (degenerate).

Unpack the 77-bit message via `unpack77`. Check `i3` (bits 75..77) and
`n3` (bits 72..74):
- `i3 > 5` → reject (invalid type).
- `i3 == 0 AND n3 > 6` → reject (invalid free-text subtype).
- `i3 == 0 AND n3 == 2` → reject (quirky subtype).

If `unpack77` fails → reject.

### Step 12: subtract on success

If `lsubtract`, call `subtractft8(dd0, itone, f1, xdt, .false.)` which
mutates `dd0` in place to remove the decoded signal. The `.false.` arg
disables the time-refinement-during-subtract (which is only used at the
outer-decode-loop sub_ft8b pass).

### Step 13: SNR estimation

Two parallel SNR estimates:

**`xsnr` (sync-based):**
- `xsig = sum(s8(itone(i), i)^2)` — signal power at the decoded tones.
- `xnoi = sum(s8((itone(i)+4) mod 7, i)^2)` — noise estimate using a
  different-tone position (the "+4 mod 7" trick — pick a tone the
  signal isn't on).
- `arg = xsig/xnoi - 1`; if > 0.1, `xsnr = 10*log10(arg) - 27`. The
  `-27` term is the calibration offset (matches WSJT-X reporting
  convention).

**`xsnr2` (baseline-based):**
- Same numerator (`xsig`).
- Denominator: `xbase * 3.0e6` where `xbase` came from the per-bin
  `sbase` baseline (passed in by the caller, originally computed by
  `sync8`/`get_spectrum_baseline`).
- Same `-27` offset.

If NOT `nagain`, prefer the baseline SNR (`xsnr = xsnr2`). If `nagain`
(operator re-decode request), keep the sync-based SNR.

Floor `xsnr` at `-24 dB`.

### Step 14: final sanity check

If `nsync <= 10` AND `xsnr < -24` → reject (likely false decode).

Otherwise return success: `nbadcrc = 0`, `msg37` populated, `xsnr` set,
`itone` filled, `iaptype` indicates which AP pass succeeded (or 0 for a
regular pass).

## The four bit-metric variants — why four?

This is the most paraphrased-over piece of `ft8b` in derivative work,
worth nailing down:

- **Variant A (single-symbol):** Standard incoherent demod. Robust at
  high SNR. Fails when symbol-by-symbol noise wipes out per-bit
  margins.
- **Variant B (2-symbol coherent):** Sums *complex* amplitudes of
  adjacent symbols. Picks up coherent multi-symbol energy when the
  channel is stable across ~80 ms. Helps at low SNR but the
  per-bit decision is now over 64 candidates (more confusion if
  channel phase rotates).
- **Variant C (3-symbol coherent):** Same trick over ~120 ms. Helps
  even more at very low SNR with a stable channel. Sensitive to
  multipath / Doppler.
- **Variant D (bit-by-bit normalized single-symbol):** Variant A's
  numerator divided by the max of the two competing terms — a
  saturation-bounded LLR. Helps when one or two bits have wildly
  different scales than the rest (which would dominate after
  normalization in variant A).

The decoder tries each variant *independently* in passes 1-4. Whichever
one finds a valid codeword first wins (the loop returns on success).
The four variants are *complementary* — a signal that fails A might
succeed B; a signal that fails B might succeed D.

## What wsjtr's docs paraphrase or miss

1. **Two sequential dt refinements, not one.** Mainline does
   `±10 samples` (coarse-baseband), then frequency-refine, then
   `±4 samples` (fine-baseband). Wsjtr collapses to one dt sweep in
   its docs. The second `±4` sweep is small but matters near the
   decode threshold — it picks up a few extra dB of effective SNR.
2. **The asymmetric downsample guard (1.5 baud below, 8.5 baud above)
   is non-symmetric on purpose** — the 8 tones extend *above* the
   carrier. A symmetric extraction would either truncate the highest
   tone or include 7 baud of pure noise below the lowest.
3. **`max_iterations = 30` for BP is set in `ft8b`, not in
   `decode174_91`.** Mainline `decode174_91` also has its own
   `maxiterations = 30` hardcoded; they happen to match, but the
   semantic source is here. Pancetta should set both consistently.
4. **AP magnitude is `1.01 * max(|llra|)`, not just `max(|llra|)`.**
   The 1.01 multiplier ensures locked bits are *strictly* louder than
   any soft bit, so BP can't ever flip them. Wsjtr's docs don't note
   this; it's a small but load-bearing factor.
5. **The Gray map is `(0, 1, 3, 2, 5, 6, 4, 7)`** — verify this is the
   one pancetta uses. It's the standard FT8 8-FSK Gray code.
6. **`nsync <= 6` early-bail, `nsync <= 10` + SNR < -24 dB late-bail.**
   Two different hard-sync gates at two different points in the
   pipeline. Wsjtr's docs treat sync gate as a single threshold.
7. **The "`+4 mod 7`" noise-tone estimate** for SNR. This is the
   "pick a tone that isn't the signal" trick. The `mod 7` is a typo
   in the original — should arguably be `mod 8` since there are 8
   tones — but in practice "+4 mod 7" maps {0..6} → {4,5,6,0,1,2,3}
   and tone 7 maps to 4 (which is fine — still not the signal tone).
   Pancetta should probably use `(itone + 4) mod 8` for correctness,
   though the difference is statistical noise.
8. **Variant C uses `512 = 8^3` candidates per 3-symbol window.** This
   is the most expensive variant by far; pancetta should profile
   before defaulting it on.
9. **AP type 6 (RR73) only ever runs as part of npasses ≥ 8.** Per the
   `nappasses` table, only `nQSOProgress = 3` and `4` reach AP pass 4.
   For nQSOProgress=5 (after sending RR73 yourself), AP type 6 is not
   in the schedule — pancetta should match.
10. **The `naptypes` matrix is contest-conditional** in a way that's
    easy to miss: for `ncontest = 7` (Hound) with `iaptype = 5`, the
    pass `cycle`s out (skips entirely) — Hounds don't look for "73"
    messages because they're in a fox-and-hound exchange, not a
    standard QSO.
11. **Subtraction during the inner loop uses `lrefinedt = .false.`.**
    Time-refinement-during-subtract is reserved for the outer-loop
    `subft8b` re-pass (see ft8_decode spec). This is a performance
    optimization — the inner subtract is cheap; refining at every
    inner subtract would be wasteful.

## Conflict with pancetta's existing mechanisms

- Pancetta's soft demod: verify which variant(s) we use. The 4-variant
  parallel decode is a known headroom item; pancetta currently uses
  ~1-2 variants depending on configuration.
- Pancetta's AP wiring is partial. The `nappasses` / `naptypes` table
  schedule is *the* AP heuristic; matching it would replicate
  mainline's AP behavior cleanly.
- The two-step dt refinement and the second frequency twiddle are
  worth verifying in pancetta — if we collapsed these into one step,
  we're leaving a small but non-zero recall gap.
- The `(+4 mod 7)` SNR noise-tone is a minor difference — pancetta's
  SNR is calibrated elsewhere; matching the formula isn't critical
  but consistency helps for cross-tool A/B testing.

## Estimated Rust port effort

- Soft demod (variants A-D + normalization): ~250 LOC of Rust.
- AP pass scheduler + apmask injection: ~200 LOC.
- Fine-sync / fine-frequency twiddle / SNR estimation: ~150 LOC.
- The whole `ft8b` analog: ~600-800 LOC of Rust, leveraging existing
  LDPC/OSD infrastructure.
- Sessions: 3-4. The 4-variant soft demod is the biggest item; the
  AP-pass schedule can land separately.

## Implementation notes for the implementer thread

- The 4-variant soft demod loop is highly parallel — each variant is
  independent. Rust can run them in 4 threads if hot enough, or just
  serially since they share the same `cs` and `s8` inputs.
- AP injection: build a single `apply_ap(iaptype, ncontest, mycall,
  hiscall, apsym, aph10)` function that returns `(apmask: [bool; 174],
  llr_override: [f32; 174])`. Keep it pure and tested with fixtures
  per contest × iaptype.
- The `naptypes(nQSOProgress, pass_index)` lookup table: encode as a
  `[[Option<u8>; 4]; 6]` Rust const, with the contest-conditional
  guards as a separate function.
- The `nsync <= 6` early-bail saves a lot of compute on noise
  candidates — pancetta should match this exactly. Don't skip it
  thinking the LDPC/OSD pass will reject anyway; the savings are
  large.
- For the four LLR variants, the normalize-to-unit-variance step
  matters — mainline divides by sigma (not by sigma^2). The scale
  factor 2.83 is approximately the standard deviation of a unit
  Gaussian (sqrt(2*ln(7)) ≈ 1.97 — actually no, 2.83 is a tuned
  constant). Pancetta should treat it as a fact, not a derivation.
- Subtraction (`subtractft8`): see the separate spec for the full
  algorithm; it's roughly a synthesize-reference-then-LPF-then-subtract
  loop. The `lrefinedt = .false.` flag is the cheap path; the
  `.true.` path adds a 3-point parabola fit over `(-90, 0, +90)`
  sample offsets to find the best dt before subtracting.
