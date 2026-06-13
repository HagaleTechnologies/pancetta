# Algorithm spec: Symbol-to-symbol phase fine sync (`fine()`)

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — `fine()` around lines 2480-2630, invocation
  inside `one_iter1` around lines 2654-2670. Constants
  `fine_thresh = 0.19`, `fine_max_off = 2`, `fine_max_tone = 4` at
  lines 105-107.
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

After coarse sync locates a candidate at (hz_bin, symbol_offset)
resolution and `search_both` refines to sub-bin via correlation
maximization, `fine()` adds one more refinement: it uses the
**symbol-to-symbol phase progression** of the detected tones to
estimate residual frequency error in fractions of a bin and residual
time offset in single samples. The mechanism is mathematically elegant
and cheap — it works directly off the already-extracted 79×8 complex
`m79` array — and produces sub-Hz frequency precision plus
single-sample time precision, both of which significantly improve
subsequent soft-demod LLR quality and (especially) subtraction
residual cleanliness.

The two refinements are derived from the same observation: an FFT
bin holds an integer number of cycles per FFT window only if the true
frequency lies exactly at the bin center *and* the FFT window starts
exactly at the symbol boundary. Any deviation appears as a per-symbol
phase rotation:

- **Frequency error** rotates the phase by a constant amount
  symbol-to-symbol. Averaged across 78 symbol-to-symbol transitions
  (weighted by the strength of the detected tone), this gives a
  noise-resistant estimate of fractional-bin Hz.
- **Time offset error** also rotates phase, but the rotation amount
  depends on the tone: a higher tone has more cycles per symbol, so
  a fixed sample offset rotates its phase more than a lower tone.
  Comparing successive symbols of different tones extracts the
  early-vs-late time-offset signal from this differential rotation.

The time-offset path is intentionally **direction-only** (early /
on-time / late, not magnitude) because the high-tone phase rotation
is ambiguous past π — at tone 7 (62.5 Hz in 200-sps), a 1-sample
offset is already ~π/8 of phase, and 2 samples is ~π/4; trying to
recover an exact sample count from a wrapped phase is unreliable.
ft8mon's compromise: detect the *direction* of the offset error
robustly, take a small step (`fine_max_off = 2` samples), and let
subsequent iterations converge.

## Algorithm description (PROSE ONLY)

### Inputs
- A 79×8 complex per-symbol FFT array `m79` at the candidate's
  current (hz, off) estimate. Same array used by the soft demod.
- `bin0` — the bin index of the candidate's lowest tone (bin 4 at
  25 Hz in the 200-sps working space; the actual decoder argument is
  the bin offset relative to the candidate's frequency).

### Outputs
- `adj_hz` — estimated Hz adjustment to add to the candidate's
  current frequency. Positive means "true frequency is higher than
  current estimate".
- `adj_off` — estimated sample offset adjustment to **subtract** from
  the candidate's current time offset. Positive means "the FFT
  windows started too early"; negative means "too late".

### Steps

#### Step 1: Identify the most-likely tone at each symbol

For each of 79 symbols, determine the tone index `sym[i]` and pull
out the complex bin value `m79[i][sym[i]]`:
- For Costas symbols (`i < 7`, `36..42`, `72..78`), `sym[i]` is the
  *known* Costas tone — no guessing. This anchors the phase
  measurement at high confidence.
- For data symbols, `sym[i]` is the **strongest-magnitude** tone
  (best-guess at the true tone). At weak SNR this may be wrong on
  some symbols; the per-symbol weighting `symval[i] = |m79[i][sym[i]]|`
  downweights weak/uncertain symbols when averaging.

Record `symphase[i] = arg(m79[i][sym[i]])` and `symval[i]`.

#### Step 2: Estimate frequency error from average symbol-to-symbol phase delta

For each transition `i → i+1` in `0..78`:
1. Compute `d = symphase[i+1] - symphase[i]`.
2. Unwrap `d` to the range `[-π, +π]` by adding/subtracting `2π`.
3. Weighted accumulation: `sum += d × symval[i]`, `weight_sum += symval[i]`.

The weighted mean `err_rad = sum / weight_sum` is the per-symbol
phase advance attributable to frequency error.

Convert from radians per symbol to Hz: `err_hz = (err_rad / (2π)) / 0.16`
where `0.16` is the FT8 symbol period in seconds. Set `adj_hz = err_hz`.

Sign convention: a positive `err_hz` means the measured phase advances
faster than the bin-center prediction → true frequency is higher than
current estimate → add `err_hz`.

#### Step 3: Estimate time offset direction from differential phase progression

The frequency-corrected per-symbol phase residual depends on tone in
a tone-direction-dependent way:

- If `off` is **too small** (FFT started too early), the FFT window
  caught the trailing portion of the previous symbol. The phase
  advance from previous to current symbol reflects the *previous*
  tone's cycle count rather than the current tone's. Concretely:
  - If the current tone is **higher** than the previous tone but
    `off` is too early, the current symbol's phase advances *less*
    than expected (the FFT saw mostly the lower previous tone).
    Observed phase difference `d < 0`.
  - If the current tone is **lower** than the previous tone but
    `off` is too early, the current symbol's phase advances *more*
    than expected. Observed `d > 0`.
- If `off` is **too large** (FFT started too late), the FFT window
  caught the leading portion of the next symbol. The relationship
  flips:
  - Current tone higher, `off` too late → `d > 0` (saw mostly the
    higher next tone).
  - Current tone lower, `off` too late → `d < 0`.

Net rule: if successive (phase difference, tone difference) pairs
are **positively correlated**, `off` is too high. If **negatively
correlated**, `off` is too low.

For each transition `i = 1..78`:
1. Compute `d = symphase[i] - symphase[i-1]`, then subtract `err_rad`
   (correct for the frequency-error estimate from Step 2), then
   unwrap to `[-π, π]`.
2. Classify based on tone direction `sym[i] - sym[i-1]`:
   - **Higher tone** (`sym[i] > sym[i-1]`):
     - `d > 0` AND `sym[i] ≤ fine_max_tone`: count as late, add
       `d / |sym[i] - sym[i-1]|` to `late`, increment `nlate`.
     - `d < 0` AND `sym[i-1] ≤ fine_max_tone`: count as early, add
       `|d| / |sym[i] - sym[i-1]|` to `early`, increment `nearly`.
   - **Lower tone** (`sym[i] < sym[i-1]`):
     - `d > 0` AND `sym[i-1] ≤ fine_max_tone`: count as early.
     - `d < 0` AND `sym[i] ≤ fine_max_tone`: count as late.
   - Equal tones (`sym[i] == sym[i-1]`): skipped (no signal).

Per-tone normalization `d / |sym[i] - sym[i-1]|` ensures large tone
jumps don't dominate the average.

#### Step 4: Decision rule

Average the accumulators: `early /= nearly`, `late /= nlate` (if
denominators nonzero).

Decide direction by majority:
- **Early** (`nearly > 2 × nlate`): set
  `adj_off = round(32 × early / fine_thresh)`, clamp to
  `+fine_max_off`.
- **Late** (`nlate > 2 × nearly`): set
  `adj_off = -round(32 × late / fine_thresh)`, clamp to
  `-fine_max_off`.
- Neither dominates → `adj_off = 0`.

The `32` factor is samples-per-symbol at 200 sps (the working rate
of `fine()`). The `/fine_thresh` factor converts the average phase
fraction to a sample count by an empirical scale; `fine_thresh = 0.19`
approximates the phase-per-sample at the mid-tone (bin 4) range.

#### Step 5: Call site

`one_iter1` calls `fine(m79, 4, adj_hz, adj_off)` and then conditionally
applies the adjustments:
- `if (do_fine_hz == 0) adj_hz = 0;`
- `if (do_fine_off == 0) adj_off = 0;`
- **Sanity guard**: only apply if `|adj_hz| < 6.25/4` (less than a
  quarter-bin) and `|adj_off| < 4` samples. Larger adjustments
  indicate the candidate is on the wrong sync entirely; better to
  not move than to chase a wrong-tone artifact.
- If accepted: `best_hz += adj_hz`, `best_off += round(adj_off)`,
  clamp `best_off ≥ 0`, then **re-extract `m79`** at the new (hz, off)
  via `shift200` + `extract`.

### Numerical constants (facts, not expression)
- `fine_thresh = 0.19` — empirical scale relating mean phase
  fraction to sample offset.
- `fine_max_off = 2` — clamp on `adj_off` in samples (per iteration).
- `fine_max_tone = 4` — only use tones up to index 4 in the
  early/late classification. Higher tones have too many cycles per
  symbol for unambiguous phase extraction at 1-2 sample offsets.
- Per-symbol period: 0.16 seconds (160 ms).
- Samples per symbol at 200 sps: 32.
- Direction-majority threshold: `nearly > 2 × nlate` (and vice
  versa).
- `do_fine_hz`, `do_fine_off`: bool gates, both default 1.
- Sanity caps on accepted adjustments: `|adj_hz| < 1.5625 Hz`,
  `|adj_off| < 4 samples`.

### Edge cases
- **Zero weight sum** in Step 2 → divide-by-zero. Source assumes
  non-empty input but a defensive clamp `weight_sum = max(weight_sum, eps)`
  is recommended.
- **Symbol periods of equal tone** (`sym[i] == sym[i-1]`) — skipped
  entirely in Step 3 because no tone-direction signal exists.
- **All early or all late, no balance** — if `nlate == 0`, the
  "early" branch fires with no comparison required. This is correct
  because phase-difference statistics are one-sided.
- **Wrong-tone identification at very weak SNR** — `sym[i]` for
  data symbols is the strongest-bin guess, which is often wrong at
  weak SNR. The `symval[i]` weighting in Step 2 downweights these,
  but Step 3 has no such weighting. Acceptable because Costas
  symbols still anchor the estimate at known tones, and weak-SNR
  data-symbol misidentifications average out.
- **Re-extraction after adjustment** — `one_iter1` re-extracts
  `m79` after applying `adj_hz` / `adj_off`. The subsequent soft
  demod sees the refined alignment. Without re-extraction, the
  adjustment is "noted" but not actually used.
- **Iterative refinement** — `fine()` is called once per candidate
  in ft8mon, not iteratively. The single application typically gets
  within sub-Hz / sub-sample of the true alignment. Iterating would
  catch second-order errors but adds compute cost.

## Conflict with pancetta's existing mechanisms

Pancetta's fine-sync stage (per CLAUDE.md and the `pancetta-ft8`
crate structure) likely uses a correlation-maximization search
(`second`/`third` stages from `search_both`-equivalent). Adding
`fine()` is **additive** — it runs after the correlation search has
already produced a sub-bin estimate and tightens it further using
phase information that the correlation search doesn't exploit
directly.

The mechanism is mathematically distinct from anything else in the
ft8mon spec collection: sub-bin Costas
(`spec-ft8mon-sub-bin-costas.md`) operates on magnitudes and brute-
force enumerates sub-bin shifts; `fine()` operates on phase and
*derives* the residual directly. They compose: sub-bin Costas
narrows down to a ~1.5 Hz × 40 ms grid, then `fine()` polishes to
~0.1 Hz × 1 sample within that grid cell.

Interaction with the Gaussian-ramp subtract spec
(`spec-ft8mon-gaussian-ramp-subtract.md`): subtraction quality is
highly sensitive to (hz, off) precision because the subtracted
waveform's phase must match the received waveform's measured phase.
`fine()`'s sub-Hz adjustment is exactly what makes the subtraction
clean enough to expose weaker layered signals. Land the ramp spec
first to get *some* subtraction working; then layer `fine()` on top
to improve residual quality.

Interaction with the three-soft-decoder-ensemble spec: `fine()` runs
**before** the ensemble. All three soft decoders see the refined
`m79`. The phase-coherence prior in `c_soft_decode` specifically
depends on accurate `m79` phase, so `fine()`'s contribution is
multiplied by the ensemble benefit.

False-positive impact: minimal. `fine()` only refines an already-
identified candidate; it cannot introduce new false candidates. The
sanity caps on accepted adjustments prevent runaway corrections that
might lock onto sidelobes.

## Estimated Rust port effort
- ~150-200 LOC in `pancetta-ft8/src/decoder/fine.rs` (new file).
- 1-2 sessions: (S1) port + unit test using a synthesized signal
  with known (hz, off) offsets from the bin center; (S2) wire into
  the decoder's fine-sync stage after the existing correlation
  search, eval on hard-200.

## Implementation notes for the implementer thread

- Make `do_fine_hz` and `do_fine_off` `Ft8Config` fields so each can
  be independently A/B tested. Defaults: both on.
- The sanity-cap check (`|adj_hz| < 1.5625 && |adj_off| < 4`) is
  load-bearing. Without it, a candidate at the wrong (hz, off) entirely
  (e.g. locked onto a sidelobe) can request a huge correction and the
  re-extracted `m79` will be even further off. Keep this check; if
  the cap fires, leave the candidate where it is.
- Re-extraction after adjustment is required — the LLR vector
  derived from the un-refined `m79` is no longer valid for the
  refined estimate.
- `fine()` operates at 200 sps (the post-`down_v7_f` working space).
  If pancetta's fine-sync stage operates at full sample rate (12 kHz),
  the math constants change (`32` becomes `1920`; samples-per-symbol
  at the working rate). Match the working rate of whatever stage
  consumes the refined values.
- The Costas-anchored tones in `sym[i]` are the load-bearing accuracy
  signal. Don't optimize them away — even if "we already know they're
  Costas tones", their phases must enter the average for the phase-
  drift estimate to be statistically efficient. The 21 Costas
  symbols provide ~25% of the symbol-to-symbol transitions, all at
  known-correct tones; the remaining 58 data transitions are noisier.
- Tier-gating: `fine()` is cheap (one pass over m79, ~150 ops).
  Always enable for all tiers.
- Eval target: subtraction residual cleanliness (FP rate reduction
  in pass-2) and weak-signal recall on the slot-edge / band-middle
  buckets where sync precision matters most. Compose with sub-bin
  Costas only after measuring each independently.
- Optional second-order refinement: re-running `fine()` after one
  application is a cheap way to catch nonlinear residual error.
  ft8mon does not iterate; pancetta could prototype.
