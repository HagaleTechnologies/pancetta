# Algorithm spec: MTD staged decoder (early-pass STD + final-pass MTD orchestration)

## Source attribution
- Origin: WSJT-X Improved by Uwe Risse, DG2YCB
  (https://sourceforge.net/projects/wsjt-x-improved/)
- Primary documents (traceability only, NOT quoted):
  `Release_Notes.txt` lines ~124–128 (v3.0.0 — MTD introduction
  and 2-Stage/3-Stage option),
  `Release_Notes.txt` lines ~145–155 (v3.0.0 250924 — a8 in MTD
  3-Stage),
  `Release_Notes.txt` lines ~202–213 (v3.0.0 251101 — full a8 in MTD,
  AP-type summarization, MTD/STD hash-table coordination),
  Third-party explainer: "Understanding FT8 Decoder Settings in
  WSJT-X 3.1 improved" (https://www.asahi-net.or.jp/~vj5y-tkur/ft8/
  wsjtx_31improved_article_en.html) — provides the `nzhsym` ↔ wall-clock
  mapping summarized in the constants section below.
- Fortran source paths cited for traceability only, NOT to be read by implementer:
  `wsjtx/lib/ft8_decode.f90` (top-level dispatcher; selects STD vs MTD
  paths),
  `wsjtx/lib/ft8/ft8d.f90` (single-threaded decoder driver — STD),
  `wsjtx/lib/ft8/ft8b.f90`, `wsjtx/lib/ft8/ft8c.f90` (per-pass / per-
  candidate driver pieces),
  multithreaded path lives in the same dir but is invoked from a
  different scheduler entry; the dispatcher in `ft8_decode.f90`
  decides which path runs at which `nzhsym`.
- License: GPL-3.0.
- Reader date: 2026-06-08.
- Reader thread: clean-room extraction (prose only).
- Upstream lineage: exclusive to WSJT-X Improved v3.0.0 (Sep 2025). Not
  in WSJT-X mainline. JTDX has a different concurrency model that does
  not generalize.

## Purpose

A 15-second FT8 receive window is filled gradually. Standard WSJT-X
runs its decoder once at the end of the window. WSJT-X Improved's
v3.0.0 introduction of the multithreaded decoder (MTD) allows multiple
passes over a *partial* audio buffer to begin while the rest of the
buffer is still being filled. The decoder thus races the clock to
surface decodes as soon as they're solvable.

The release notes describe two operator-selectable orchestration modes
on top of this:

- **2-Stage**: a lightweight single-threaded pass (STD) at an early
  audio-buffer fill point, followed by a full multithreaded pass (MTD)
  at a late point. Captures the bulk of decodes early; tidies up
  with the heavy pass at the end.
- **3-Stage**: STD pass at an early point, another STD pass at a
  middle point, MTD pass at the latest point. The middle pass picks
  up decodes that need slightly more audio than the early pass had.

Per release notes (verbatim, line ~127): "With the MTD options
'2-stage' and '3-stage' both decoders can be intelligently combined,
resulting in the best FT8 decoding performance to date."

Per a third-party operator's article: "2-Stage delivers about 99.5%
of the decoding yield of 3-Stage, while requiring far less computing
power."

The operational benefits are two-fold:

1. **Early display of decodes.** As soon as the early STD pass finds a
   signal, the operator sees it on screen — typically 1.5 to 2.5 seconds
   before the end of the window.
2. **Better total yield.** The multipass / a7 / a8 work in the final
   MTD pass runs over a *cleaner* residual because the early STD pass
   has already subtracted its decodes. The final pass therefore
   surfaces more weak signals.

The mechanism stacks with a8 (see spec-wsjtx-improved-a8-decoding.md):
a8 runs as part of the early STD pass, and its decodes appear on
screen 0.5–1 second earlier than they would in the final pass.

## Algorithm description (PROSE ONLY — no code)

### Inputs

The orchestration runs at the receive-window timeline level. Inputs:

- **Streaming audio buffer**: 12 kHz mono float, accumulated across
  the receive window. At any moment the buffer contains the audio
  from the start of the window up to the current wall-clock time.
- **Operator-selected orchestration mode**: `Off` (single MTD final
  pass only), `Early` (single MTD pass at an early time),
  `Normal` (single MTD pass at the standard time),
  `Late` (single MTD pass at a late time), `2-Stage` (STD early + MTD
  late), `3-Stage` (STD early + STD mid + MTD late). The release
  notes mention "2-stage" and "3-stage" as the headline new options;
  the third-party article documents Early/Normal/Late as additional
  single-pass MTD modes.
- **The standard receive-window clock**: wall-clock seconds since the
  window started. Used to determine when each pass fires.
- **The full configured-decoder context**: `Ft8Config`, message hash
  tables, baseline computation, AP machinery (a7, a8). Both STD and
  MTD passes share most of this context.

### Outputs

A single combined decode list per window, presented to the operator
incrementally as each pass completes. Each decode carries:
- Standard decode fields (text, dt, freq, snr).
- Provenance tag indicating which pass produced it
  (`StdEarly`, `StdMid`, `MtdFinal`, or unified `Standard` if pancetta
  prefers a coarser taxonomy).

### Steps

1. **Begin receive window.** Wall-clock t=0. Start accumulating audio.
   The decoder schedules its passes at the wall-clock times described
   by the operator's selected mode (see Numerical constants below).

2. **At t ≈ 11.8s (Stage 1 — STD early pass):**
   - The audio buffer now holds the first ~11.8 s of the 15 s window.
   - Run an STD pass: search the spectrum, decode candidates via
     BP/OSD, subtract accepted decodes from the residual.
   - For each accepted decode: emit immediately to the operator's
     display (this is what produces the early-decode UX).
   - This pass is structurally identical to a standard FT8 decode
     pass — it is the same code, just invoked on a shorter audio
     buffer.

3. **(3-Stage only) At t ≈ 13.2s (Stage 2 — STD mid pass):**
   - The audio buffer now holds ~13.2 s.
   - Run a second STD pass against the *current residual* (i.e., the
     audio buffer minus the subtractions from Stage 1).
   - For each accepted decode: emit immediately to the operator.

4. **At t ≈ 14.1 to 14.4s (Stage 3 — MTD final pass):**
   - The audio buffer now holds almost the full 15 s window.
   - Run the MTD pass: a multipass-style decode that uses thread
     parallelism to explore more candidate locations / more LLR
     variants / deeper OSD than the STD passes did.
   - Include a7 (cross-sequence AP) and a8 (early-QSO-state AP) in
     this pass, as standard for the heavy decoder.
   - Subtract decodes; emit to operator.

5. **Per-pass deduplication.** Any decode found in Stage 1 that the
   subsequent passes re-discover at a similar `(dt, freq, text)` is
   discarded. Implementation: maintain a per-window decode set keyed
   on `(call1, call2, freq_bin)` (with a small frequency tolerance).

6. **Per-pass hash-table coordination.** Per release notes line ~212
   (v3.0.0 251101, verbatim): "Better communication between the hash
   tables of both decoders (MTD and STD), further reducing the
   likelihood of hash collisions." This means: callsign hashing for
   the 22-bit hashed-callsign message types must be coherent between
   passes. The hash table observed by Stage 1 must also be observed by
   Stage 3 so that a Stage-3 decode of a hashed callsign that Stage 1
   resolved earlier in the same window can be displayed in full.

7. **Window boundary.** At t=15s, the audio for the next window starts
   accumulating; the current window's final pass results are committed.

8. **Operator-mode mapping** (single-pass modes for comparison):
   - `Early`: Stage 3 (MTD) only, fired at t≈13.8s. No prior STD passes.
   - `Normal`: Stage 3 (MTD) only, fired at t≈14.1s.
   - `Late`: Stage 3 (MTD) only, fired at t≈14.4s.

### Numerical constants (facts, not expression)

- **`nzhsym` ↔ wall-clock mapping** (per third-party article based on
  WSJT-X internals; these are widely-known protocol-internal values):
  - `nzhsym = 41` ≈ 11.8 seconds (Stage 1 / 2-Stage early / 3-Stage
    early).
  - `nzhsym = 46` ≈ 13.2 seconds (Stage 2 of 3-Stage).
  - `nzhsym = 48` ≈ 13.8 seconds (Early single-pass MTD).
  - `nzhsym = 49` ≈ 14.1 seconds (Normal single-pass MTD; also
    2-Stage final).
  - `nzhsym = 50` ≈ 14.4 seconds (Late single-pass MTD; also 3-Stage
    final).
- **Effective yield comparison** (per third-party article, attributed
  to DG2YCB): 2-Stage delivers ~99.5% of 3-Stage yield at much lower
  CPU cost. This makes 2-Stage the recommended default for most
  operators.
- **MTD thread parallelism**: per release notes, multi-core (any
  modern machine). Specific thread count is implementation detail
  (likely `num_cpus / 2` or similar; not stated in release notes).
- **STD vs MTD**: STD is single-threaded; MTD parallelizes the
  candidate-search and decode work across threads. The MTD's
  algorithm internals are not described in release notes — only the
  operator-visible orchestration is.
- **Early-display time savings**: per release notes line ~152
  (v3.0.0 250924, a8 specifically) and per the article (general
  Stage 1 benefit), decodes appear on screen 0.5 to 2.5 seconds
  earlier than the window-end-only decode would surface them.

### Edge cases

- **Slow host can't finish Stage 2 in time.** The third-party article
  notes this verbatim: "On weaker computers, the nzhsym=46 pre-pass
  used by 3-Stage may fail to finish in time for the main MTD
  decoding at nzhsym=50. In that case, the most important final
  MTD step cannot be performed." Mitigation: 2-Stage (only Stage 1
  + Stage 3) or single-pass MTD modes. This maps directly to
  pancetta's Slow tier — default to single-pass MTD-equivalent
  there.
- **Audio buffer not yet full when Stage 1 fires.** Stage 1 fires at
  t=11.8 s; the audio buffer is 11.8 s long, not 15 s. The decoder
  must accept a shorter input and handle the missing samples
  gracefully. (For Costas-array sync, this is fine — the sync points
  are within the first ~12 s of the window. For full-frame demod,
  the demod sees the trailing edge of each candidate; a signal whose
  trailing tones haven't arrived yet is rejected by sync-quality.)
- **Operator changes mode mid-window.** Use the new mode starting
  with the next window. Do not re-process the current window.
- **a8 + Stage 1.** a8 fires as part of Stage 1 (the early STD pass).
  Per release notes, this is where the "0.5–1 second earlier display"
  benefit comes from.
- **a7 + final pass.** a7 fires as part of Stage 3 (the final MTD
  pass), because it needs the previous-window opposite-parity decode
  table — that table is only fully populated after the prior window
  completes.
- **Decode appearing in Stage 1 but not in final pass.** The Stage 1
  decode is committed. Even though the final pass didn't re-confirm
  (probably because Stage 1's subtraction left a clean residual),
  the operator's display is correct — the decode was valid.
- **Subtraction in Stage 1 that hurts final pass.** Possible: Stage 1
  decodes incorrectly (false positive), subtracts a wrong waveform,
  and the final pass now sees corrupted residual. The standard FP
  filters apply to Stage 1 decodes, so this is rare; the failure mode
  is the same as any false subtraction in multipass decode.

## Conflict with pancetta's existing mechanisms

- **Major architectural change.** Pancetta currently decodes once per
  full 15 s window. Implementing 2-Stage requires the audio pipeline
  to expose partial buffers at scheduled wall-clock times. The
  coordinator (`pancetta/src/coordinator/pipeline.rs` or similar) is
  the natural home for the scheduler.

- **Threaded decoder (MTD).** Pancetta's current decoder is
  single-threaded. Implementing the full MTD machinery is a
  substantial work item. The 2-Stage / 3-Stage benefit is partially
  available even with a single-threaded "MTD" — the operational
  yield gain comes from the staged residual-cleaning effect, not
  specifically from threading. Pancetta could ship "staged decode"
  first (single-threaded but multi-stage) and add real threading
  later.

- **Conflict with the existing decoder API.** Today's decoder API is
  "give me a full window of audio, return all decodes." The staged
  approach requires "give me a partial window, return decodes so
  far, and remember the residual for the next stage." This is a
  significant API change with downstream effects on tests, the
  coordinator, and the autonomous operator.

- **Synergy with a8.** a8's headline benefit (early display) only
  applies under a staged scheme. Without staged decoding, a8 collapses
  to "yet another end-of-window AP pass." The 2-Stage architecture
  is *enabling infrastructure* for a8.

- **Synergy with hb-091 (scoped fast path) / hb-216 (tier
  classification).** Slow tier: single-pass MTD-equivalent (i.e.,
  pancetta's current behavior). Moderate tier: 2-Stage. Fast tier:
  2-Stage by default; 3-Stage if the operator opts in. The Slow-tier
  scoped-fast-path already constrains the decoder to a minimal
  configuration; the staged decoder fits naturally as a per-tier
  policy.

- **Interaction with FDR (see spec-wsjtx-improved-fdr.md).** FDR
  applies to every decoded message regardless of which stage
  produced it. The per-pass provenance tagging gives FDR additional
  context if its thresholds want to differ by pass (e.g., be
  slightly stricter on Stage 1 decodes because the audio buffer is
  shorter), though no released WSJT-X Improved version does this.

- **Interaction with multi-stream Tx and SmartFrequencyAllocator.**
  Pancetta's TX scheduling reads the latest decode list to make
  decisions. With staged decoding, the decode list is *incrementally
  available* during the window. The TX scheduler could in principle
  start making TX decisions at Stage 1 time, ~2.5 s earlier than
  today. This is a meaningful operational improvement for
  autonomous QSOs (faster reply turnaround).

## Estimated Rust port effort

- **Phase A: Staged decode without threading.** Refactor the decoder
  to accept partial audio buffers and to maintain residual state
  across stages. Implement 2-Stage as the default orchestration.
  - Decoder API change: `decode_partial(audio, stage_id, residual_in)
    -> (decodes, residual_out)`. ~300 LOC.
  - Coordinator changes: schedule three decoder calls per window
    instead of one; thread the residual between them. ~150 LOC.
  - Tests: synthetic two-signal WAV where one signal is fully
    contained in the first 11.8 s and the other is across the
    boundary. Confirm Stage 1 catches the first, Stage 3 catches the
    second. ~120 LOC tests.
  - Total Phase A: ~570 LOC + ~120 LOC tests.
  - Sessions: 3–4.

- **Phase B: True multithreading (MTD-equivalent).** Parallelize the
  per-candidate decode work using `rayon` or equivalent. This is
  primarily a performance win, not a yield win — Phase A already
  captures the staged-residual benefit.
  - Internal decoder threading: candidate iteration over a thread
    pool. ~200 LOC + tests.
  - Sessions: 2–3.

- **Phase C: 3-Stage option.** Add the middle STD pass at
  `nzhsym=46`. Mostly a config switch + scheduler entry.
  - ~30 LOC + ~50 LOC tests.
  - Sessions: 1.

- **Total**: ~800 LOC + ~250 LOC tests across 6–8 sessions.

## Implementation notes for the implementer thread

- **Ship Phase A first.** The staged-residual benefit is the bulk of
  the operational win. Real threading is a perf improvement;
  reserve it for after the staged architecture is stable.

- **Decoder API is the hard part.** The change from
  "give-me-a-window, return-decodes" to "give-me-a-partial-window
  + residual-state, return-decodes + new-residual-state" cascades
  through many call sites. Plan the API carefully before writing
  code. A `DecoderState` struct that carries the residual + hash
  tables across stages is the natural shape.

- **Hash-table coherence is non-optional.** Per release-notes line
  ~212, hash collisions across stages can cause hashed-callsign
  decodes to fail. The `DecoderState` struct must hold the unified
  hash table; both early and final passes write to and read from
  the same instance.

- **Wall-clock scheduling.** Use the coordinator's timer
  infrastructure (`pancetta/src/coordinator/`). Do not block on
  audio arrival inside the decoder; the coordinator schedules
  decoder invocations at wall-clock times, and the decoder runs
  asynchronously with whatever audio has accumulated.

- **Per-pass provenance.** Add `DecodeProvenance` variants
  `StdEarly`, `StdMid`, `MtdFinal`. Use these for telemetry and
  debugging. The autonomous operator and FDR may want to treat
  Stage 1 decodes slightly differently (e.g., higher confidence
  threshold for autonomous-TX decisions because the audio buffer
  was shorter).

- **Tier defaults**: Slow → single end-of-window pass (current
  behavior). Moderate → 2-Stage. Fast → 2-Stage by default, 3-Stage
  opt-in.

- **a8 integration.** a8 runs as part of Stage 1. The wiring is
  straightforward once Phase A's API is in place. Until Stage 1
  exists, a8 has no useful early-display benefit (it runs end-of-
  window like everything else).

- **a7 integration.** a7 runs as part of Stage 3. It needs the prior
  window's opposite-parity decode table, which is only complete
  after Stage 3 of the prior window. The cross-sequence
  SequenceHistory (per spec-wsjtr-cross-sequence-a7.md) is the
  natural home for this.

- **The 4th-pass-after-a7 mechanism.** That mechanism (see
  spec-wsjtx-improved-4th-pass-after-a7.md) is itself a Stage 4
  if a7 produced decodes. Net pipeline: Stage 1 STD → (Stage 2 STD)
  → Stage 3 MTD (with a7) → Stage 4 if a7 was productive.

- **Testing strategy.** Build a multi-window QSO fixture that
  exercises every stage: window N has the operator's previous Tx,
  decoded in window N's Stage 1; window N+1 has the partner's reply,
  decoded in Stage 1 via a8 (early), with a7 in Stage 3 mining for
  additional QSO companions, and an optional Stage 4 if a7 produced
  decodes.

- **Citation hygiene.** Cite as "inspired by WSJT-X Improved v3.0.0
  MTD + 2-Stage/3-Stage orchestration (DG2YCB)" in the journal entry.
  The staged-residual pattern is a general iterative-decoding idea;
  the specific operator-facing orchestration modes (Off / Early /
  Normal / Late / 2-Stage / 3-Stage) and the `nzhsym` choices are
  the WSJT-X Improved instantiation. Pancetta's implementation is
  independent.
