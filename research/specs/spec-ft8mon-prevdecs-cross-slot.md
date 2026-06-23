# Algorithm spec: Cross-slot subtraction via `prevdecs`

## Source attribution
- Origin: ft8mon
- File path: `ft8.cc` — `prevdecs_` state at line 438, constructor at
  lines 442-466, application around lines 828-851 inside `go()`, the
  entry-point plumbing at lines 3030-3082, `ft8.h` struct cdecode.
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

Within a single slot, ft8mon does intra-slot spectral subtraction:
strong signals are decoded first, subtracted, then a second pass
finds weaker signals previously masked. The decode + subtract loop is
inside `go()` and operates on the same slot buffer.

Across slots, ft8mon exposes an interface for the caller to pass in a
**list of signals decoded in a previous slot** that should be
subtracted from the new slot's buffer **before** decoding starts. The
mechanism: each `cdecode` carries the 174-bit corrected codeword
(from LDPC output), the previous-slot frequency `(hz0, hz1)`, and
the previous-slot time offset `off`. At the start of the new slot's
decode, before any candidate search, ft8mon re-runs the same
subtraction code that the intra-slot loop uses, after first
fine-tuning the `(hz, off)` parameters because the previous slot's
estimates may no longer perfectly match the new slot's audio.

Why this matters: a strong continuously-transmitting signal (e.g. a
contest station on a stable frequency calling CQ every slot) emits
in slot N+1 with nearly the same `(hz, off)` it had in slot N. Its
intra-slot subtraction in slot N+1 will eventually find and subtract
it — but only after several passes have already spent compute budget
on candidate sweeps that the strong signal would otherwise dominate.
Pre-subtracting it from the buffer means the very first coarse-sync
pass in slot N+1 sees a cleaner spectrum, finds the *weak* signals
that the strong signal would have masked, and runs them first.
Empirically this is a 1-2 dB sensitivity win on contest-busy bands.

The pancetta context: the QSO state machine and the autonomous
operator already know which signals were decoded in the previous
slot. Feeding them back as `prevdecs` is a coordinator-level wiring
change with no decoder-internal changes beyond exposing the
subtraction primitive.

## Algorithm description (PROSE ONLY)

### Inputs
- The new slot's audio buffer (just received, not yet decoded).
- A list of `cdecode` entries from previous slots. Each entry
  contains:
  - `hz0`, `hz1` — the decoded signal's start and end frequencies in
    the original (pre-rate-reduction) frequency space. Equal when
    drift was not modeled.
  - `off` — the decoded signal's time offset in seconds (slot-
    relative).
  - `bits[174]` — the LDPC-corrected codeword bit vector.

### Outputs
- The decoder's internal `nsamples_` buffer has every applicable
  `prevdec` subtracted from it before any pass-0 candidate search.
- After all `prevdecs` are processed, `samples_` (the input to each
  pass) is reset to `nsamples_` so all subsequent passes also see
  the cleaned buffer.

### Steps

1. **Caller assembles a `prevdecs` list** from the QSO state +
   autonomous operator + DX cluster + cqdx.io feed. Recommended
   sources:
   - Last slot's decode list filtered to "still relevant" callsigns
     (CQ-callers continue calling; QSO partners continue exchanging).
   - Operator-defined hunt list — frequencies the operator marked as
     "always pre-subtract" (e.g. a known local interferer).

2. **At entry**, the decoder constructor accepts the `prevdecs` list
   and stashes it.

3. **After per-thread rate reduction (stage 1 of the rate-reduction
   spec)**, the decoder applies the same `delta_hz` correction to
   every `prevdecs` entry's `(hz0, hz1)`:
   - `prevdecs[i].hz0 -= delta_hz`
   - `prevdecs[i].hz1 -= delta_hz`
   This keeps the previous-slot frequencies aligned with the
   new (reduced-rate) frequency space.

4. **Before pass 0**, iterate over `prevdecs`. For each entry whose
   `hz0` falls within this thread's `[min_hz_, max_hz_]` slice:
   a. **Reconstruct 79 symbol indices from the 174 bits** by calling
      `recode()` — the same function used in the intra-slot
      subtract path. This re-applies the gray-encode and re-inserts
      the three Costas blocks at positions 0..6, 36..42, 72..78,
      yielding the same 79-symbol sequence that LDPC's output
      implies.
   b. **Refine `(hz, off)` to the new slot's audio** by calling
      `search_both_known(samples_, rate_, re79, best_hz_in, best_off_in, &best_hz_out, &best_off_out)`.
      `search_both_known` is a Costas-pattern-aware joint search:
      given a known 79-symbol sequence, sweep `(hz, off)` over a
      small window and pick the maximum-correlation point. This is
      essential because (a) the transmitting station may have small
      frequency drift between slots, and (b) the receiver's clock
      may differ from slot to slot due to sample-rate roundoff.
      Without refinement, the subtraction phase would be off and
      cancellation would be poor.
   c. **Subtract** using the standard `subtract()` function with the
      refined `(best_hz, best_off)` and the reconstructed `re79`.
      This is the same Gaussian-ramp subtraction described in the
      `spec-ft8mon-gaussian-ramp-subtract.md` spec — no special
      handling for the cross-slot case beyond the (hz, off)
      refinement.
   d. **Increment `any` counter** so that we know at least one
      cross-slot subtraction fired.

5. **If any cross-slot subtraction fired**, copy `nsamples_` (which
   has had all `prevdecs` subtracted) into `samples_`. This makes
   the cleaned buffer the starting point for pass 0 (which otherwise
   would have used the unmodified received buffer).

6. **Standard intra-slot passes run as usual** on the cleaned buffer.
   The intra-slot subtract loop continues to fire for any new strong
   decodes within the current slot, layering on top of the cross-slot
   pre-cleaning.

### Numerical constants (facts, not expression)
- No new tunable constants beyond those already in the subtraction
  spec. Inherits `subtract_ramp = 0.11` and the search window
  parameters from `search_both_known`.
- Number of passes is `npasses_two = 3` (vs `npasses_one = 3` when no
  `prevdecs` are supplied). In ft8mon as written these are equal, but
  the entry-point logic
  `int npasses = nprevdecs > 0 ? npasses_two : npasses_one;`
  exists explicitly so the operator can budget more or fewer passes
  when prior-slot context is available.

### Edge cases
- **Frequency out of this thread's slice** — entries with
  `hz0 < min_hz_` or `hz0 > max_hz_` are silently skipped. Each
  thread handles only the signals in its band, which means a
  `prevdec` at e.g. 1500 Hz is processed by exactly one thread (the
  one whose slice covers 1500 Hz). Sharing the cleaned buffer
  across threads is not needed because each thread already has its
  own `samples_` / `nsamples_` pair.
- **Refinement search divergence** — if the previous-slot signal is
  not present in the new slot (the station stopped transmitting),
  `search_both_known` will not find a strong correlation peak and
  the subtracted waveform will have wrong `(hz, off)` and small
  amplitude. The subtraction is mostly a no-op (amplitude near zero
  from the FFT magnitude measurement), so the spurious subtraction
  is benign rather than destructive.
- **Slot N decode without subsequent slot N+1 cleanup** — the
  receiver may decide to not pass slot N's decodes into slot N+1
  (e.g. because slot N+1 is a TX slot for pancetta and the receiver
  is muted). Handle by simply passing an empty `prevdecs` list.
- **TTL on prevdecs** — a signal heard in slot N has decreasing
  probability of still being on the same frequency in slot N+k for
  large k. Recommended TTL: 2-3 slots (30-45 seconds) before purging.
- **Subtracting our own TX** — pancetta's own outbound signal will
  also be present in the received audio. Adding it to `prevdecs`
  would subtract it cleanly, which is the *right* behavior for a
  transceiver in semi-duplex mode (e.g. where the receiver is
  picking up the transmitter's tail). The QSO state machine knows
  what pancetta just sent.

## Conflict with pancetta's existing mechanisms

Pancetta's coordinator currently:
- Decodes slot N → emits decodes upward.
- Subtracts strong signals **within** slot N for the multi-pass loop.
- Discards the slot N audio buffer when slot N+1 audio arrives.

There is no current cross-slot pre-subtraction. Adding it is a
straightforward coordinator change:

1. The coordinator already holds a per-slot decode list. Promote
   this to a rolling buffer with TTL.
2. At slot N+1 decode start, build a `prevdecs` list from:
   - Recent (within TTL) decodes from slot N and N-1.
   - Active QSO partner's most-recent decode.
   - Operator's static hunt list, if any.
3. Pass this list to the decoder.

The decoder needs to expose the subtraction primitive (cleanly
factored out of `subtract.rs` so the coordinator can drive it). The
decoder also needs to accept the `prevdecs` list as part of its
input parameters (or via a new method like `decode_with_prevdecs`).

Interaction with hb-216 hardware tier classifier: `prevdecs` adds
fixed-cost subtraction work proportional to the number of cross-slot
signals. On Slow tier, limit to ~5 entries; Moderate ~20; Fast no
limit. The coordinator's existing per-slot budget mechanism (the
deadline plumbed through `go()`) absorbs any overrun.

Interaction with the Gaussian-ramp subtract spec: this spec is a
pure *consumer* of the cleaner subtraction — it does not change the
subtraction mechanism. Land the ramp-subtract spec first, then this
one; the cleaner intra-slot subtraction makes the cross-slot
subtraction also cleaner.

Interaction with the hint mechanism (`use_hints`): both are caller-
side hooks for feeding QSO-state context into the decoder, but at
different stages. Hints clamp LLR bits *during* decode; `prevdecs`
subtract waveforms *before* decode. They compose: a QSO partner's
known callsign goes into `hints2`, *and* the QSO partner's last
decoded slot goes into `prevdecs`.

## Estimated Rust port effort
- ~150-200 LOC for the coordinator-side rolling-decode buffer with
  TTL and filtering logic. Lives in
  `pancetta/src/coordinator/` (likely a new
  `prevdecs.rs` submodule).
- ~50 LOC for plumbing through the decoder entry point.
- The subtraction primitive itself is reused from the Gaussian-ramp
  subtract spec — no new decoder-internal code.
- 1-2 sessions: (S1) coordinator-side rolling buffer + TTL + a unit
  test that feeds two slots of synthetic audio with the same signal
  in both and confirms the slot-2 decode is cleaner with `prevdecs`
  vs without; (S2) eval on a multi-slot hard-200 subset, measuring
  weak-signal recall improvement when a strong signal is consistently
  present in both slots.

## Implementation notes for the implementer thread

- Data shape:
  ```text
  struct PrevDec {
      bits: [u8; 174],           // LDPC-corrected codeword
      hz0: f32,                  // frequency at slot start (drift hi)
      hz1: f32,                  // frequency at slot end
      off_s: f32,                // time offset in seconds, slot-relative
      decoded_at: Instant,       // for TTL
  }
  ```
  Rolling buffer in coordinator with TTL ~30s, dedup by callsign.
- The `recode()` function (LDPC bits → 79-symbol sequence) needs to
  be available outside the decoder's hot path. Factor it into a
  shared util.
- `search_both_known` is a separate spec topic — it's the joint
  (hz, off) search for a *known* symbol sequence. Pancetta may
  already have an equivalent for testing the OSD output. If not,
  it's ~50 LOC of nested loop over a small window.
- TTL: ft8mon's source does not include a TTL — the Python caller is
  expected to manage it. Pancetta should manage it explicitly to
  avoid stale `prevdecs` causing wrong-frequency subtraction (mostly
  benign per the edge-case analysis, but wasteful).
- Coordinator-side composition with the QSO state machine: the QSO
  partner's last-slot decode should always be in `prevdecs`,
  regardless of TTL, because pancetta is mid-QSO with them and they
  are guaranteed to keep transmitting on the same frequency.
- Performance note: each `prevdec` adds a 79-symbol FFT (for the
  fine-tune in `search_both_known`) plus the subtraction work
  itself. On Fast tier this is negligible (<1 ms per entry); on Slow
  tier with ~20 entries it could add 100+ ms before pass 0. The
  Slow-tier cap suggestion above (~5 entries) keeps this bounded.
- Eval: synthesize a corpus with one consistently-transmitting
  strong signal across 5 consecutive slots and ~5 weaker signals
  scattered across the band. Measure slot-2-onwards recall with and
  without `prevdecs` enabled. Expected lift: 2-5 percentage points
  on weak-signal recall in the slots where the strong signal is
  pre-subtracted.
- This is also the natural place to wire pancetta's own outbound TX
  signal subtraction (if the receiver is unmuted during pancetta's
  TX slots — e.g. for full-duplex or for receive-during-tail
  configurations). The TX symbol sequence is known exactly from the
  message; package it as a `PrevDec` and pass through the same hook.
