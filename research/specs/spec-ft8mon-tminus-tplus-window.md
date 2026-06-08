# Algorithm spec: Wide coarse-sync time window (`tminus = 2.2 s`, `tplus = 2.4 s`)

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — constants at lines 73-74, derivation of
  `si0`/`si1` inside `go()` around lines 820-824, slot tail padding
  at lines 801-819.
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

Most FT8 decoders search a relatively narrow time window around the
nominal slot start — typically ±0.5 seconds — on the assumption that
both the receiver and the transmitter are reasonably well synchronized
to NTP or GPS time. This works well for the median case but fails for
two specific classes of transmitter:

1. **Senders with badly-misconfigured clocks** — operators running on
   unsynced laptops or running custom code with a buggy time source.
   Drift of several seconds is not unheard of.
2. **Senders on intermittent or marginal time sources** — battery-
   powered field portable operations using GPS that hasn't acquired,
   then transmitting "close enough" by visual estimate.

ft8mon takes the opposite approach: search a **wide** time window
that extends from −2.2 s to +2.4 s around the nominal slot start
(`tminus = 2.2`, `tplus = 2.4`). The search itself is no more
expensive per candidate (it's just a wider sweep at the coarse-sync
stage), but it requires the slot audio buffer to contain
enough samples to cover the late-end search range (`+ tplus × rate`
samples past the nominal 79-symbol end), which ft8mon arranges by
padding with random samples drawn from elsewhere in the slot if the
incoming buffer is short.

The hb-217 pancetta batch identified slot-edge negative-dt as the
worst recall bucket (48.3% recall on 1376 truths). That bucket
contains exactly the senders whose `dt` falls outside the typical
±0.5 s search window. ft8mon's wide window directly addresses this
gap.

## Algorithm description (PROSE ONLY)

### Inputs
- The slot audio buffer of length at least
  `start + tplus × rate + 79 × block + block` samples, where
  `start` is the sample index of "0.5 seconds into the slot" (the
  FT8 nominal start time), `rate` is the working sample rate, and
  `block` is samples per symbol.
- `tminus` (seconds before nominal start to search) and
  `tplus` (seconds after nominal start).

### Outputs
- Symbol-index sweep bounds for the coarse Costas-sync search:
  - `si0 = max(0, (start - tminus × rate) / block)` — earliest
    symbol position to try.
  - `si1 = (start + tplus × rate) / block` — latest symbol position
    to try.
- Coarse-sync evaluates Costas correlation at every `si` in
  `[si0, si1]`, candidate frequency bin by candidate frequency bin,
  and emits the strongest as candidates for fine sync.

### Steps

1. **Verify slot length.** Check whether
   `start + tplus × rate + 79 × block + block > samples.size()`.
   If yes, the slot is too short for the late-end search.

2. **Pad if necessary.** Compute the number of extra samples needed:
   `need = start + tplus × rate + 79 × block - samples.size()`.
   Round up to a whole second (`rate` samples) so the FFT planner
   cache stays warm. Generate `need` extra samples by sampling
   uniformly at random from existing positions in `samples` —
   importantly, **not zeros**, because zeros at the tail would
   create artificial spectral edges that the coarse-sync FFT would
   see as a wideband impulse, contaminating the soft demod's noise
   distribution. Random-resampling from the slot itself reproduces
   the slot's noise statistics in the padded region.

3. **Compute search bounds in symbol indices:**
   - `si0 = (start - tminus × rate) / block`, clamped to ≥ 0.
   - `si1 = (start + tplus × rate) / block`.

4. **Run coarse Costas-sync across `[si0, si1]`.** This is the
   standard coarse-sync sweep; the only effect of wide
   `tminus`/`tplus` is that `(si1 - si0)` is larger and the sweep
   visits more candidate offsets. Per candidate the cost is
   unchanged.

5. **Fine sync at the strongest candidates.** The wide coarse search
   only affects which `si` values get tried; once a candidate has a
   strong Costas score, fine sync refines around that `si` with a
   small ±2-symbol window (per ft8mon's `second_off_win` /
   `second_off_n` parameters).

### Numerical constants (facts, not expression)
- `tminus = 2.2 seconds` — earliest start time relative to nominal.
- `tplus = 2.4 seconds` — latest start time relative to nominal.
- Total search width: 4.6 seconds. Compared to typical decoder
  ±0.5 s (1.0 s total), this is ~4.6× wider.
- Symbol period at 12 kHz: 160 ms (1920 samples). The
  `[si0, si1]` range is roughly
  `[start_symbol - 13.75, start_symbol + 15.0]` symbol indices.
- Padding rule: round up to whole seconds of `rate` samples; fill
  with uniformly-random samples drawn from existing slot positions.

### Edge cases
- **`si0 < 0`** clamped to 0 in the source. Negative symbol indices
  are unrepresentable; the search just starts at the buffer head.
- **`si1` past buffer end** — handled by the padding step. Without
  padding, the coarse-sync FFT at the late `si` values would read
  past the buffer end, producing garbage. With padding, the FFT
  reads valid noise-like samples and produces a noise-floor score
  for that `si` — no false positives.
- **Pure noise in the padded region** — the random-resample padding
  is a *valid* noise model only if the slot is long enough to have
  enough variation. For very short slots (<1 second), random
  resampling reproduces the same handful of samples and produces a
  non-random spectrum. Acceptable for FT8 because slot length is
  ~14 seconds; padding lengths are at most ~3 seconds.
- **NTP-drifted *receiver*** — ft8mon assumes the receiver itself
  has correct time and is searching for misaligned *transmitters*.
  If both ends drift the same way (e.g. a portable operation
  running on the same drifted clock), the wide search still finds
  them since it's relative.
- **Spurious decodes at extreme dt** — wide search increases the
  raw candidate count by ~4.6×, raising the absolute false-positive
  load slightly. CRC + the existing FP filters absorb this without
  recalibration in practice.

## Conflict with pancetta's existing mechanisms

Pancetta's decoder time search window (per CLAUDE.md and Batch 30
results) appears to use a relatively standard ±0.5 s range. Widening
to ±2.2/±2.4 s is a one-parameter change at the coarse-sync stage,
plus the padding logic. The hb-217 slot-edge negative-dt bucket
(48.3% recall, 1376 truths) is exactly the population this targets;
expected lift from widening alone is several percentage points.

The padding logic is the main implementation concern:
- Pancetta's input pipeline (`pancetta-audio`) emits a fixed-length
  per-slot buffer of `slot_duration_s × sample_rate` samples. If
  this is exactly `14.6 × 12000 = 175200`, there is already enough
  tail to cover `tplus = 2.4`; no padding needed at the input
  level.
- If pancetta's slot buffer is tighter (e.g. exactly the FT8 message
  duration of 12.64 s), the decoder needs to either request a wider
  buffer from the audio pipeline or do the random-resample padding
  itself.

Interaction with the sub-bin Costas spec
(`spec-ft8mon-sub-bin-costas.md`): both modify the coarse-sync
sweep. The sub-bin spec adds 16× frequency × time sub-bin
oversampling; widening the time window adds another ~4.6× in time
search range. Multiplicatively, coarse sync becomes ~73× more
expensive than the bin-aligned ±0.5 s baseline. This may push the
decoder past its deadline on Slow tier. Suggested staging:
- Slow tier: tminus/tplus default 0.5 / 0.5 (current narrow), no
  sub-bin.
- Moderate tier: tminus/tplus 1.0 / 1.0 (modestly wider), sub-bin
  half-resolution (8 instead of 16 sub-positions).
- Fast tier: tminus/tplus 2.2 / 2.4 (ft8mon defaults), sub-bin
  full 4×4 = 16 positions.

The hb-216 tier classifier hook is the right place to gate this.

Interaction with the autonomous responder's `recently_responded_to`
back-off: wider time search may surface more decodes per slot,
including more dupes of the same callsign at varying `dt` — these
are already deduplicated downstream by the per-callsign 60-second
back-off, so no new dedup logic needed.

Interaction with the `slot_parity` plumbing in
`pancetta/src/coordinator/tx.rs`: a decoded message at very early
or very late `dt` could be a remnant of the *previous* or *next*
slot. ft8mon does not distinguish this case — every decode is
attributed to "the current slot". Pancetta's `slot_parity` already
correctly classifies which parity the *transmission* originated
from; widening the time search may surface slot-N+1 transmissions
during slot N decode (early `dt > 13`) which should be reclassified
to slot N+1's parity. This is a small downstream wiring issue, not
a decoder change.

## Estimated Rust port effort
- ~50 LOC: configurable `tminus` / `tplus` parameters, padding
  helper, plumbing through the coarse-sync sweep.
- 1 session: implement + unit test with a synthetic signal placed
  at dt = -2.0 s and dt = +2.0 s, confirming both decode at wide
  window but not at narrow window. Eval on hard-200 slot-edge
  bucket.

## Implementation notes for the implementer thread

- Make `tminus` and `tplus` `Ft8Config` fields. Default the values
  to match pancetta's existing search (likely 0.5 / 0.5) so this
  spec lands as opt-in until tier-gating wires it on for Fast.
- Padding implementation: if the input buffer is shorter than
  `start + tplus × rate + 79 × block + block`, pad in-place. Use
  a deterministic seeded RNG (e.g. `StdRng::seed_from_u64(slot_index)`)
  so re-running on the same slot produces identical results — useful
  for regression tests. ft8mon's source uses
  `std::default_random_engine` which is implementation-dependent.
- The random-resample padding is **subtly important**: do not use
  zero padding, repeat-last-sample padding, or reflection padding.
  Zero/reflection create artificial spectral edges; repeat-last
  creates a DC plateau that swamps the noise floor estimate. Only
  the random-resample-from-slot strategy correctly reproduces the
  slot's noise statistics in the tail.
- The padding length is rounded up to a whole second — this is for
  FFT-planner cache stability and is not algorithmically required.
  Pancetta can omit the rounding if its FFT planner doesn't have
  the same cache cost.
- For the slot-edge negative-dt bucket specifically, also confirm
  pancetta's audio capture *captures* the pre-nominal-start audio.
  If the audio buffer starts exactly at the nominal slot edge with
  no pre-roll, no amount of wide search will help — the early-`dt`
  symbols are gone. Audio capture should pre-roll by at least
  `tminus + 0.5` seconds.
- Eval target: hb-217 slot-edge negative-dt bucket (48.3% recall
  baseline, 1376 truths). Headroom estimate: 5-10 percentage points
  on that bucket, ~0.5-1.0 percentage points on full hard-200.
  Pair with audio-capture pre-roll if not already present.
- Compose carefully with `spec-ft8mon-sub-bin-costas.md` — the
  multiplicative compute cost is non-trivial. Recommended order:
  land this spec first (cheap, narrow win on the slot-edge bucket),
  measure the recall lift, then tier-gate sub-bin.
