# Algorithm spec: WSJT-X mainline ft8_decode — outer decode loop and multipass orchestrator

## Source attribution

- Origin: WSJT-X mainline (K1JT et al., upstream)
- Repository: https://sourceforge.net/p/wsjt/wsjtx/ci/master/tree/
- Mirror: https://github.com/saitohirga/WSJT-X
- File (traceability only; NOT quoted): `lib/ft8_decode.f90` (a
  Fortran *module*, not `lib/ft8/ft8_decode.f90` — note path: it's
  at the top of `lib/`, not inside `lib/ft8/`).
- Companions: `lib/ft8/sync8.f90`, `lib/ft8/ft8b.f90`,
  `lib/ft8/subtractft8.f90`, `lib/ft8/ft8_a7.f90`, `lib/decoder.f90`
  (the caller that instantiates and invokes this module).
- License: GPL-3.0
- Reader date: 2026-06-08
- Reader thread: clean-room extraction (prose only)

## Purpose

`ft8_decode` is the outer loop of the WSJT-X FT8 decoder. It coordinates:

- The 3-stage early/middle/final decoding strategy (11.8 s / 13.5 s /
  14.7 s into the slot — corresponding to `nzhsym ∈ {41, 47, 50}`).
- Early-decode subtraction carry-over: signals decoded at nzhsym=41 get
  subtracted from the buffer before the next stage's sync pass.
- Multi-pass invocation of `sync8` + `ft8b` per stage (1, 2, or 3
  passes depending on configuration).
- Per-pass `syncmin` and `lsubtract` variation.
- Time-budget bail-out: if the slot-relative wall clock exceeds
  thresholds (13.4 s for early, 14.3 s for middle), bail out before
  finishing the candidate list.
- Cross-sequence "a7" decodes — re-trying decodes from the *previous*
  slot using stored partial-call data (deferred chronological context).
- Duplicate suppression across passes within a slot.

This is the file that defines the FT8 decoder's *temporal personality* —
when it decodes early vs late, when it gives up, what it carries over.

## Note on naming

The Fortran file is named `ft8_decode.f90` and defines a *module* named
`ft8_decode` containing a `type :: ft8_decoder` with a `decode` method.
The actual subroutine that does the work is `decode` inside the module
(invoked via `my_ft8%decode(...)`). The caller (`lib/decoder.f90`)
instantiates a `counting_ft8_decoder` (subtype that tracks per-call
counts) and calls `decode`.

Wsjtr's docs may refer to this as "the decoder" or "the outer loop";
both are correct.

## Inputs (`decode` subroutine)

- `callback` — function pointer invoked once per accepted decode (the
  GUI uses this to display results in real time).
- `iwave(15*12000)` — raw 16-bit audio at 12 kHz. Read once into the
  internal real buffer `dd`.
- `nQSOProgress` — operator's QSO state (see ft8b spec).
- `nfqso`, `nftx` — operator QSO and TX frequencies.
- `newdat` — passed to `ft8b` to indicate dd has new data.
- `nutc` — current UTC integer (HHMMSS); used to detect slot transition.
- `nfa`, `nfb` — frequency search bounds (Hz).
- `nzhsym` — number of half-symbol spectrogram columns available so
  far. **This is the key stage discriminator.**
  - `nzhsym = 41` → early decode (~11.8 s into slot)
  - `nzhsym = 47` → middle decode (~13.5 s into slot)
  - `nzhsym = 50` → final decode (~14.7 s into slot)
  - The math: `nzhsym * NSTEP / 12000 = nzhsym * 480 / 12000 = nzhsym * 40 ms`
  - `41 * 40ms = 1640 ms` into the spectrogram, which combined with the
    earliest-possible-start offsets gives ~11.8 s of audio used.
- `ndepth` — 1/2/3 decoder depth (BP-only / BP+OSD-uncoupled /
  BP+OSD-coupled).
- `emedelay` — earth-moon-earth delay offset (used for EME modes;
  shifts dt by 2 s if non-zero — not relevant for HF FT8).
- `ncontest`, `nagain`, `lft8apon`, `lapcqonly`, `napwid` — contest
  mode, re-decode-request, AP enable/CQ-only, AP frequency window
  (all passed through to `ft8b`).
- `mycall12`, `hiscall12` — operator callsigns.
- `ldiskdat` — `.true.` if decoding from a saved WAV file (no time
  budget; processes everything).

## Outputs

Per accepted decode, the callback is invoked with:
- `sync`, `snr`, `dt`, `freq`, `decoded` (37-char message), `nap`
  (which AP type succeeded), `qual` (quality in `[0, 1]`).

## Numerical constants

- `MAXCAND = 600` — passed to `sync8` as the candidate buffer cap.
- `MAX_EARLY = 100` — max number of early-decode results stored for
  carry-over.
- Stage thresholds: `nzhsym ∈ {41, 47, 50}`.
- Time-budget bails (only when `.not. ldiskdat`, i.e., real-time):
  - Early stage: bail if `tseq >= 13.4` (13.4 s into slot).
  - Middle stage: bail if `tseq >= 14.3` (14.3 s into slot).
- Pass count:
  - `ndepth = 1`: `npass = 2`.
  - `ndepth = 2` or `3`: `npass = 3`.
- Per-pass `syncmin`:
  - `ndepth <= 2`: `syncmin = 1.6`.
  - `ndepth = 3`: `syncmin = 1.3`.
  - `nzhsym == 41` (early stage): `syncmin = 2.0` (override —
    early stage is conservative).
- Per-pass `ndeep` (passed to `ft8b` as `ndepth`):
  - Pass 1: `ndeep = ndepth`, except if `ndepth = 3`, then `ndeep = 2`.
  - Passes 2 and 3: `ndeep = ndepth`.
  - This means at full `ndepth = 3`, the first pass intentionally
    uses *less* OSD aggressiveness than the second and third passes.
- Subtraction:
  - Pass 1: `lsubtract = .true.`.
  - Pass 2: `lsubtract = .true.` (only runs if pass 1 found ≥1 decode).
  - Pass 3: `lsubtract = .true.` (only runs if pass 2 found ≥1 decode).
- Mid-stage early-decode-subtract refinement: `lrefinedt` is `.true.`
  if `ndepth >= 3`, else `.false.`. (See subtractft8 for what `lrefinedt`
  means — it adds a 3-point parabola fit to find optimal dt before
  subtracting.)
- Early-decode dt gate for subtraction: only subtract if
  `xdt_save(i) - 0.5 < 0.396` (i.e., the early decode happened before
  ~0.4 s relative dt — late early decodes can't be safely subtracted
  before middle-stage processing).
- Middle-stage buffer splice: when carrying middle stage to final
  stage, `dd(1:n) = dd1(1:n)` (preserved subtracted buffer from
  middle stage) and `dd(n+1:) = iwave(n+1:)` (fresh audio for the
  new samples), where `n = 47 * 3456 = 162432` samples (= 47 *
  3456 samples = 13.536 s).
  - Note: `3456 = NSTEP * 7.2` — wait, `NSTEP = 480`, so `3456 = 480*7.2`
    doesn't work as integer. Actually `3456 = 8*432 = NSPS * 1.8`?
    Let me recompute: 47 * 3456 = 162432. And `162432 / 12000 = 13.536 s`.
    The `3456` is `12 * 288` — possibly a stride related to a different
    spectrogram setting. **Read the source if you need this exact**;
    for the spec, treat `n = 162432` as a magic constant marking the
    "where the early-stage audio ended".

## Algorithm description (prose only)

### Stage gating

The decoder is invoked once per nzhsym threshold crossing. There are
three threshold crossings per FT8 slot:

1. **Early decode at nzhsym = 41 (~11.8 s).** Goal: fast first-look,
   subtract anything decoded so the middle stage has cleaner audio.
   `syncmin = 2.0` (conservative — only act on very strong candidates).
   No AP at this stage (`npasses = 4` always forced).
2. **Middle decode at nzhsym = 47 (~13.5 s).** Performs the early-
   decode-subtractions on a fresh copy of the buffer (`dd1`), then
   bails out and lets the next caller (nzhsym = 50) finish. The
   middle stage's job is mostly to do the subtraction, not to decode
   afresh.
3. **Final decode at nzhsym = 50 (~14.7 s, end of slot).** Full
   decode: 3 passes with full AP if enabled, all early-stage decodes
   carried over and re-subtracted on the final buffer.

The state-machine is complex because of `dd` vs `dd1` (two buffers,
one with subtractions applied, one without), and because of the
time-budget bail paths.

### Slot-transition reset

If `nutc != nutc0` OR `nzhsym == 41` (the first invocation of a slot):
- Move previously saved "a7" cross-sequence data from slot k=1 to k=0
  (the "previous slot" → "previous-previous slot" promotion).
- Reset slot k=1's saved arrays.
- Update `nutc0 = nutc`.

This is what makes the `a7` cross-sequence pass (Step 7 below) work —
it has access to *one slot earlier* worth of decodes for context.

### Stage 1: early decode (nzhsym ≤ 47)

`dd = iwave` (read fresh audio).
`dd1 = dd` (back up).

If `nzhsym == 41`: reset `ndecodes = 0`, clear `allmessages` and
`allsnrs`.

Then proceed to the main pass loop (next section). Subtractions happen
inline (`lsubtract = .true.` for all passes), mutating `dd` but not
`dd1`. Decodes are saved to `xdt_save`, `f1_save`, `itone_save`,
indexed `1..ndec_early`.

At end of stage 1: `ndec_early = ndecodes`; jump to label `800`.

### Stage 2: middle decode (nzhsym == 47 AND ndec_early >= 1)

Carry forward the early-decode subtractions to a fresh `dd` (this is
the new audio buffer with everything that the early stage decoded
already removed). For each saved early decode:

- If `xdt_save(i) - 0.5 < 0.396`, call `subtractft8(dd, itone, f1, xdt,
  lrefinedt)`. `lrefinedt = .true.` if `ndepth >= 3`. Mark the entry
  as subtracted in `lsubtracted(i)`.
- After each subtraction, check the wall clock — bail if `tseq >= 14.3 s`
  (give the final stage time to finish).

Then `dd1 = dd` (back up the subtracted buffer for stage 3). Jump to
label `900` (skip ahead to the cross-sequence pass; final stage will
handle the rest).

### Stage 3: final decode (nzhsym == 50)

The final-stage logic is the most intricate. Two sub-paths:

**Sub-path A: `nagain = .true.`** (operator re-decode request)
- Force `dd = iwave` (don't reuse subtracted buffer).
- Narrow frequency search to `nfqso ± 20 Hz`.
- Proceed to main pass loop.

**Sub-path B: normal final decode**
- If `ndec_early >= 1`: re-splice the buffer. `dd(1:162432) = dd1(1:162432)`
  (preserve the subtracted-from-middle-stage region) and
  `dd(162433:end) = iwave(162433:end)` (fresh audio for the new
  samples that weren't available at middle stage).
- Re-apply any early-decode subtractions that weren't applied in
  middle stage (because they were filtered by `xdt - 0.5 < 0.396` or
  by the time-budget bail). These run with `lrefinedt = .true.`
  regardless of `ndepth`.

Then proceed to the main pass loop.

### Main pass loop (used in stages 1 and 3)

`npass = 3` (or `2` if `ndepth = 1`).

For `ipass = 1` to `npass`:

- `newdat = .true.` (signal `ft8b` to redo the long FFT downsample).
- Set `syncmin` per the rules above (1.3/1.6/2.0).
- `lsubtract = .true.` for all passes.
- For pass 1: `ndeep = ndepth` (or `2` if `ndepth = 3`).
- For pass 2: skip if `ndecodes == 0`. Otherwise `ndeep = ndepth`.
- For pass 3: skip if `ndecodes - n2 == 0` (no new decodes in pass 2).
  Otherwise `ndeep = ndepth`.

So passes 2 and 3 are *conditional on the previous pass having produced
new decodes*. The intuition: if pass 1 cleaned out the easy signals,
pass 2 reveals signals that were previously masked; if pass 2 reveals
new signals, pass 3 may reveal even more (waterfall effect).

Within each pass:

1. Call `sync8(dd, ifa, ifb, syncmin, nfqso, MAXCAND, candidate, ncand,
   sbase)` → list of candidates.
2. For each candidate `icand` in `1..ncand`:
   - Extract `(sync, f1, xdt)`.
   - Compute `xbase = 10^(0.1 * (sbase(nint(f1/3.125)) - 40.0))` — the
     local baseline power, converted from dB back to linear, with a
     -40 dB offset.
   - Call `ft8b(dd, newdat, nQSOProgress, nfqso, nftx, ndeep, nzhsym,
     lft8apon, lapcqonly, napwid, lsubtract, nagain, ncontest, iaptype,
     mycall12, hiscall12, f1, xdt, xbase, apsym2, aph10, nharderrors,
     dmin, nbadcrc, iappass, msg37, xsnr, itone)`.
   - On success (`nbadcrc == 0`):
     - Dedup against `allmessages(1:ndecodes)` (exact 37-char match
       suppression).
     - If new: append to `allmessages`, `allsnrs`, save `f1`,
       `xdt + 0.5` (note the +0.5 unshift), `itone` for downstream
       use.
     - Compute `qual = 1.0 - (nharderrors + dmin) / 60.0`, clamped to
       `[0, 1]` implicitly.
     - Apply EME shift if `emedelay != 0`: `xdt += 2.0`.
     - Invoke the callback.
     - Save to the `a7` table for future cross-sequence decoding.
   - Check time budget: if `nzhsym == 41` AND `tseq >= 13.4` AND not
     reading from disk → bail.

### Stage 4: cross-sequence "a7" decode pass

Runs only if all of:
- `lft8apon` (AP enabled globally).
- `ncontest != 6 AND ncontest != 7` (not Fox or Hound mode).
- `nzhsym == 50` (final stage only).
- `ndec(jseq, 0) >= 1` (have decodes from the previous slot).

This is the "deferred chronological context" path. For each previous-
slot decode:
- Skip if `f0 == -99` (sentinel for end-of-list).
- Skip if `f0 == -98` (sentinel for already-handled).
- Skip if the message contains `<` (placeholder for non-standard call —
  the original comment says "Temporary").
- Parse the message into `(call_1, call_2, grid4)` (split on spaces).
- If `grid4 == 'RR73'` or contains `+`/`-` (SNR-like markers), zero
  it out — only real Maidenhead grids are useful as AP.
- Look up `xdt`, `f1` from the saved tables.
- Compute `xbase` from the current slot's `sbase`.
- Call `ft8_a7d(dd, newdat, call_1, call_2, grid4, xdt, f1, xbase, ...,
  msg37, xsnr)`. This is a specialized one-shot decode that injects
  the previous-slot calls as AP and re-attempts a decode on the
  current slot.
- On success: invoke callback with `iaptype = 7` (the cross-sequence
  AP type marker), `qual = 1.0`.
- Save to a7 table.

This is the mechanism that catches "I missed VK3ABC's CQ in slot N
but caught their grid in slot N+1 because I knew their call already".

### Time-budget bailout

The wall-clock checks (`tseq >= 13.4` and `tseq >= 14.3`) are critical
for real-time operation. WSJT-X must finish processing slot N before
slot N+1's audio finishes recording, else the GUI falls behind. The
bail-outs are conservative — they stop processing mid-candidate-list
rather than risk skipping the next slot.

Bail-out is *only* checked when `.not. ldiskdat` — i.e., when decoding
real-time. When the operator is replaying a WAV file, all decoding runs
to completion.

## What wsjtr's docs paraphrase or miss

1. **The 3-stage timing is `nzhsym ∈ {41, 47, 50}`, not "early /
   middle / final" abstractions.** The exact numbers are load-bearing:
   they're tied to spectrogram-column counts (1640 ms / 1880 ms /
   2000 ms of spectrogram → ~11.8 s / 13.5 s / 14.7 s of audio,
   accounting for the 0.5 s slot offset).
2. **The middle stage exists primarily to do early-decode
   subtractions**, not to perform decoding itself. After applying
   subtractions, it jumps to label `900` (cross-sequence pass) and
   skips the main pass loop. wsjtr's docs sometimes describe three
   "decode" stages; mainline has one decode stage early + one subtract
   stage middle + one decode stage final.
3. **Passes 2 and 3 are conditional on the previous pass producing
   new decodes.** wsjtr may describe this as "3 unconditional passes";
   the original is gated.
4. **`syncmin = 2.0` at nzhsym=41 is a hardcoded override**, separate
   from the `ndepth`-conditioned default. Easy to miss in
   reimplementation.
5. **`ndeep = 2` for pass 1 when `ndepth = 3`** — the first pass at
   full depth still uses pass-2-level OSD aggressiveness, not pass-3.
   This is a CPU-budget heuristic: pass 1 has the most candidates;
   reserve the heaviest OSD for passes 2 and 3 where candidate count
   is lower (signals already cleaned up).
6. **Early-decode dt gate `xdt - 0.5 < 0.396`** — only early decodes
   with rough dt under ~0.4 s relative get carried into mid-stage
   subtraction. Late-arriving early decodes are deferred to final
   stage.
7. **`lrefinedt` for mid-stage subtract requires `ndepth >= 3`**; at
   `ndepth = 2` the subtract uses idt=0 (no parabola fit). The
   refined-dt subtraction is more accurate but ~3x more expensive
   (3 FFTs instead of 1).
8. **Final stage with `ndec_early >= 1` splices `dd1[1:162432]` +
   `iwave[162433:end]`.** The splice point `n = 47 * 3456 = 162432`
   is "where the middle-stage audio ended". The samples after that
   point are *new* (recorded between middle stage and final stage),
   so they must come from `iwave`, not the buffer that has
   subtractions applied.
9. **`emedelay != 0` shifts `xdt` by +2 s.** This is for EME; not
   relevant for HF FT8 but ports may need to account for it if they
   want bit-exact match.
10. **`qual = 1.0 - (nharderrors + dmin) / 60.0`** is the quality
    metric reported to the callback. The `60.0` denominator is a
    tuned scale; smaller values map to "less reliable".
11. **The cross-sequence `a7` pass is wrapped in
    `shmem_lock` / `shmem_unlock`** (per the use clause for the
    `shmem` module — though not visible in the body) — there's shared
    state across decoder invocations. Pancetta should serialize
    accordingly.
12. **`MAX_EARLY = 100`** is the cap on early-decode carry-over. If a
    band is overwhelmingly active, decodes beyond #100 in the early
    stage are forgotten by the middle/final stages.

## Conflict with pancetta's existing mechanisms

- Pancetta's outer decode loop uses `max_decode_passes` (configured at
  1, 2, or 3) which roughly aligns with mainline's `npass`. Verify the
  per-pass `syncmin` / `ndeep` variation matches mainline (this is
  what wsjtr's per-pass-variation spec covers — confirm pancetta's
  implementation respects the conditional pass 2/3 gating).
- The 3-stage timing is **NOT** currently in pancetta — pancetta
  processes the slot once at the end (~nzhsym=50 equivalent). Adding
  early-decode-then-subtract is a significant infrastructure change
  but offers real headroom: it gives the final stage a cleaner audio
  buffer for the harder signals.
- The `a7` cross-sequence path is not in pancetta. This is the
  "deferred chronological context" mechanism — would slot into the
  QSO state machine cleanly since pancetta already tracks
  per-callsign context.
- Pancetta's frequency search bounds `nfa, nfb` are usually wide
  open. Verify `xbase` is computed per-candidate using the per-bin
  baseline from `sbase` (not a global noise estimate) — this affects
  the second SNR estimate in `ft8b`.
- Time-budget bailout doesn't apply to pancetta (which runs decoder
  on a thread per slot — no hard real-time deadline within a slot).
  Skip the wall-clock checks.

## Estimated Rust port effort

- 3-stage `nzhsym` gating + buffer management (`dd` / `dd1`): ~150 LOC.
- Early-decode carry-over data structures (`itone_save`, `f1_save`,
  `xdt_save`, `lsubtracted`): ~100 LOC.
- Main pass loop with conditional passes 2/3: ~100 LOC.
- Cross-sequence `a7` pass: ~150 LOC.
- Total: ~500-700 LOC of Rust, depending on how cleanly it integrates
  with pancetta's existing coordinator.
- Sessions: 2-3 for the 3-stage gating + subtract; 1 separately for
  `a7`.

## Implementation notes for the implementer thread

- The two-buffer dance (`dd` and `dd1`) is critical. `dd` is the
  "working" buffer that subtractions mutate; `dd1` is the "preserved"
  snapshot at each stage. Mis-aliasing these causes silent recall loss.
- In Rust, use `Vec<f32>` with `.clone()` for the snapshot; the buffer
  is 180 KB, cheap to clone.
- The conditional pass 2/3 gating (skip if previous pass found nothing)
  is a meaningful CPU savings on quiet bands. Implement it.
- The per-pass `syncmin` and `ndeep` table is small; encode as a const
  lookup keyed by `(ipass, ndepth, nzhsym)`. Watch the `nzhsym = 41 →
  syncmin = 2.0` override.
- For the splice point: just use `n = 47 * 3456 = 162432` as a const.
  Don't try to derive it from `NSTEP` or other constants — the source
  doesn't either.
- Cross-sequence `a7`: see `lib/ft8/ft8_a7.f90` (separate file, ~378
  LOC). The `dt0`, `f0`, `msg0` arrays are 3D (`(MAXDEC, jseq, 0:1)`),
  indexed by `(decode_index, slot_index_in_minute, slot_pair_offset)`.
  Pancetta probably wants a simpler data structure since it's not
  multi-mode-multiplexed.
- The `nutc0 = -1` initial-value check is a Fortran-specific idiom for
  "first call ever"; in Rust use `Option<u32>` or similar.
