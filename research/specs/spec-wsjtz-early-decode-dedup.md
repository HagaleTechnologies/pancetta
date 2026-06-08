# Algorithm spec: WSJT-Z early-decode message-text deduplication across the three sub-slot decode intervals

## Source attribution
- Origin: WSJT-Z (https://github.com/sq9fve/wsjt-z) — a WSJT-X fork by SQ9FVE adding
  extended automation and a modified multi-threaded FT8 decoder pipeline.
- File path (for traceability, NOT to be quoted): `lib/ft8_decode.f90`
  (subroutine `decode` within the `ft8_decode` module; module-level `save`
  state lives near the top of the same subroutine; supporting Fox-mode
  arrays at module scope are unrelated to this mechanism).
- License: GPL-3.0.
- Reader date: 2026-06-08.
- Catalog cross-reference: Agent F (Batch 47) flagged this as the one
  decoder-relevant WSJT-Z addition not already covered by the JTDX
  catalog.

## Purpose

WSJT-X's multi-threaded FT8 decoder runs the FT8 decoder three times per
15 s slot: once at roughly 11.8 s into the slot ("early"), once at
roughly 13.5 s ("mid", with subtract-and-re-decode active), and once at
roughly 14.7 s ("final", with the additional a7 / a8 hypothesis decoders
active). The three passes operate on progressively more of the audio
buffer and use progressively more aggressive decoder settings, so the
same physical signal is normally decoded *all three times*. WSJT-X
mainline relies on the receiving GUI process to filter the resulting
triple-emission before it lands in the band-activity panel. WSJT-Z's
modification is to do the dedup *inside the decoder library*, before
the callback that ships each decoded line to the parent process, so
that only the first sighting of each unique message text in a slot is
reported. This cleans up the band-activity panel, halves to a third
the work done by downstream consumers (loggers, callsign filters, DXCC
lookups), and crucially lets WSJT-Z's automation layer (Auto-CQ /
Auto-Call) react to a CQ at the 11.8 s mark without then re-firing on
the same callsign at the 13.5 s or 14.7 s marks.

The mechanism is intentionally simple: a per-slot cache of
message-text strings, raw-byte-equal comparison, no normalisation,
no frequency or SNR keying.

## Algorithm description (PROSE ONLY)

### Inputs (to the dedup layer)

- The 37-character ASCII payload string (`msg37`) returned by the
  inner FT8 decoder (`ft8b`) for a single successful candidate.
- The CRC-pass flag (`nbadcrc`) — zero when the message has passed
  the FT8 CRC-14.
- The current half-symbol index `nzhsym` — this is the decoder's
  notion of how far into the 15 s slot the audio buffer has been
  filled, in half-symbol units. The three trigger points in WSJT-Z's
  pipeline are `nzhsym = 41`, `nzhsym = 47`, and `nzhsym = 50`.
- The UTC second of the slot (`nutc`) — used for the cache reset
  trigger, not for the dedup key.

### Outputs

- A boolean `ldupe`. When `ldupe` is true, the dedup layer
  suppresses the callback that would otherwise ship this decode to
  the GUI / parent process. The decode itself is silently dropped
  from the visible output.
- A side effect: when `ldupe` is false, the message text is appended
  to the per-slot cache for future comparisons within the same slot.

### Persistent state

The dedup uses Fortran `save` semantics inside the `decode`
subroutine. The persistent variables relevant to this mechanism are:

- `allmessages` — a one-dimensional array of `character*37`, sized
  to the parameter `MAX_EARLY = 200`. Each slot in the array holds
  one previously-seen message text.
- `allsnrs` — a parallel one-dimensional integer array of the same
  size, holding the SNR with which each cached message was first
  seen.
- `ndecodes` — the running fill count of `allmessages` for the
  current slot. (A separate `ndec_early` counter records how many
  decodes the *previous* early pass produced; it is used for
  diagnostics and not for dedup.)
- `nutc0` — the UTC second at which the cache was last reset.

These survive between calls to the `decode` subroutine, which is
how state accumulates across the three sub-slot triggers within
one 15 s slot.

### Steps (per call into the decoder)

1. **Slot-start reset.** When `nzhsym == 41` (the earliest of the
   three triggers, fired around 11.8 s into a fresh slot) the cache
   is reset: every entry of `allmessages` is overwritten with 37
   space characters, `allsnrs` is zeroed, `ndecodes` is set to
   zero, and `nutc0` is set to the new `nutc`. This is the only
   reset path; the cache does not autonomously roll over on any
   other condition.

2. **Run the inner decoder.** The candidate list is processed by
   `ft8b` exactly as in mainline. No dedup occurs before `ft8b` —
   the mechanism never suppresses a decode *attempt*, only its UI
   emission. CPU spent on duplicate decodes is unchanged.

3. **CRC gate.** Only candidates with `nbadcrc == 0` (CRC pass)
   are eligible for the dedup machinery. CRC-failed candidates
   cannot reach the cache and cannot be flagged as duplicates.

4. **Linear scan for exact match.** The decoded `msg37` is
   compared against `allmessages(1..ndecodes)` using raw 37-byte
   Fortran string equality (`.eq.`). The first equal entry sets
   `ldupe = .true.` and the scan continues to the end. There is
   no early-out; the loop always runs `ndecodes` iterations.

5. **Emit-or-suppress decision.** When `ldupe` is false the
   callback fires (one decoded line out to the calling process)
   and the message is appended: `ndecodes` is incremented,
   `allmessages(ndecodes) = msg37`, `allsnrs(ndecodes) = nsnr`.
   When `ldupe` is true the callback is skipped and the cache is
   not modified; in particular, the existing `allsnrs(...)` entry
   for that text is *not* updated even if the duplicate sighting
   has a higher SNR.

6. **Cache full.** When `ndecodes >= MAX_EARLY = 200` the
   per-candidate inner loop hits a `cycle` (Fortran's "next
   iteration") and stops adding entries. The cache does not wrap;
   it simply becomes inert for the rest of the slot. The dedup
   *check* (step 4) still runs against whatever 200 entries are
   already there, so already-seen messages remain suppressed, but
   the 201st distinct message text and every distinct message
   after it gets emitted on every pass it appears in. In practice
   200 distinct decodes per 15 s slot is well above any plausible
   real-world band density.

7. **Record early-pass count.** After the early passes (nzhsym <
   50, i.e. nzhsym = 41 and nzhsym = 47) the current `ndecodes`
   is captured into `ndec_early` for use as a diagnostic /
   downstream metric. This does not affect dedup behaviour.

### Triggers within a slot

The three pass entries into `decode` happen at `nzhsym` values
41, 47, and 50. With FT8's symbol period of 0.16 s (1920 samples
at 12 000 sps) and the half-symbol stepping that `nzhsym` counts,
these correspond approximately to:

- `nzhsym = 41` → ~11.8 s — first early decode on the partial
  audio buffer that has accumulated by that point.
- `nzhsym = 47` → ~13.5 s — second early decode, more audio
  available, more time for SIC subtraction to converge.
- `nzhsym = 50` → ~14.7 s — final decode with the full slot of
  audio and the a7 / a8 hypothesis decoders enabled.

A signal that decodes at all three triggers will produce three
calls into the dedup layer with the same `msg37`. The first call
appends, the second and third are suppressed. A signal that
decodes only at the final 14.7 s trigger appends once and is
emitted once, identical to single-pass behaviour.

## How this differs from WSJT-X mainline

Mainline WSJT-X emits each successful decode unconditionally
through the same callback path each time the multi-threaded
pipeline fires `ft8_decode%decode`. The three sub-slot passes
exist, and the same signal will normally decode in all three of
them, producing three callback events for the same `msg37`. The
*GUI process* on the receive side is responsible for filtering
those down to a single "band activity" line — typically by
keeping a per-callsign-and-frequency suppression map for the slot
and only displaying the first decode it sees. The decoder
library itself has no notion of duplicate suppression across the
three intervals.

WSJT-Z moves this dedup from the GUI into the decoder library
itself: by the time a decode reaches the calling process, it has
already been deduped against everything else seen in the same
slot. From the parent process's perspective each slot now emits
each distinct message at most once. The visible consequence in
WSJT-X's normal `decoded.txt` log file is one line per signal per
slot instead of (typically) three.

Two functionally important differences fall out of this:

- **Automation layers that react to the decoder callback no
  longer fire three times per signal.** WSJT-Z's Auto-CQ /
  Auto-Call automation reads decodes off the decoder pipe in real
  time; without this dedup it would have to implement its own
  text-equal suppression or risk responding to the same CQ three
  times in quick succession. With the dedup in the library,
  automation can treat each callback as a unique sighting.

- **The 11.8 s early decodes become first-class.** Mainline's
  GUI typically waits for the 14.7 s pass before displaying
  anything because the early passes are noisy and the GUI does
  not have a clean way to amend a previously-displayed line. With
  in-library dedup, WSJT-Z can confidently emit the 11.8 s sighting
  *immediately*, knowing that the 13.5 s and 14.7 s re-sightings of
  the same text will be suppressed automatically. This is the
  latency-reduction angle: a CQ that decodes at 11.8 s in WSJT-Z
  reaches the operator's UI almost three seconds earlier than the
  same CQ in mainline.

This is also distinct from JTDX's dedup, which lives inside the
inner decoder (`ft8b`) and operates on `(message_text,
audio_frequency, snr)` triples with a ±45 Hz frequency window —
JTDX is deduping across the candidate-search-and-LDPC loop
*within* one pass, then again at message-emission time across the
nine sub-passes of one decode cycle. WSJT-Z's mechanism is
strictly text-only, no frequency window, and operates *only*
across the three sub-slot trigger points of the multi-thread
pipeline.

## Numerical constants (facts, not expression)

- `MAX_EARLY` (cache size) = `200` entries.
- `allmessages` element width = `37` characters (the standard FT8
  decoded-text length).
- Sub-slot decode triggers: `nzhsym` = `41`, `47`, `50`.
- Approximate real-time of those triggers, given FT8's
  symbol period (0.16 s) and half-symbol stride: ~11.8 s, ~13.5 s,
  ~14.7 s into the 15 s slot.
- Cache reset trigger: `nzhsym == 41` (and only that).
- Dedup key length: full 37 ASCII bytes, no transformation.
- Dedup key comparator: Fortran `character*37` equality, i.e.
  byte-for-byte equal including trailing-space padding.
- `ndec_early` reset: zeroed at `nzhsym == 41`; captured from
  `ndecodes` at `nzhsym < 50` (used as a diagnostic only).
- CRC gate: `nbadcrc == 0` required for a candidate to interact
  with the cache at all.

## Edge cases

- **Cache full at 200.** Distinct messages beyond the 200th in
  the same slot will be emitted on every pass they decode in (no
  dedup possible because they aren't in the cache). In practice
  unreachable in normal HF band conditions; pathological worst
  case is a contest band with hundreds of simultaneous CQs.
- **No SNR-update on duplicate.** When a message text re-decodes
  at higher SNR in a later pass, the cached `allsnrs` entry is
  *not* refreshed. The first sighting's SNR is the one reported,
  even though the 14.7 s pass typically has a better SNR estimate
  than the 11.8 s pass. This is a small accuracy loss in exchange
  for the simplicity of the linear scan.
- **No frequency keying.** Two stations transmitting identical
  text (e.g. two stations both calling `CQ DX K1JT FN20`, which
  is impossible in practice but theoretically possible during a
  Fox-mode pile-up of the same callsign across multiple frequencies)
  would collide and only the first would be emitted. The mechanism
  does not consider frequency — it is text-only.
- **No callsign keying.** The dedup is on the *whole* message text,
  not on the from-callsign. Two consecutive messages from the same
  station that differ in any way ("`CQ K1JT FN20`" vs "`CQ DX K1JT
  FN20`") count as distinct and both emit.
- **Reset only at `nzhsym == 41`.** If the decoder is invoked at
  some other `nzhsym` value (e.g. due to a missed start-of-slot
  trigger), the cache from the *previous* slot persists and may
  spuriously suppress what should have been the first sighting of
  a message in the new slot. In normal pipeline operation the 41
  trigger is reliable, so this is mostly theoretical, but worth
  noting if pancetta's tick scheduler is less deterministic.
- **No interaction with Fox-mode arrays.** The module-level
  `c2fox` / `g2fox` / `nsnrfox` / `nfreqfox` / `n30fox` arrays
  used for Fox-mode pile-up handling are entirely separate from
  this dedup mechanism. They store per-callsign Fox state, not
  per-text dedup state. The two mechanisms do not interact.
- **No promotion / no amendment.** The 11.8 s sighting is final
  — there is no logic that says "if we see this text again at
  14.7 s with higher SNR, retroactively amend the displayed
  line". The first emission is the one the operator sees.

## Conflict with pancetta's existing mechanisms

Pancetta currently has two layers of message-text suppression:

1. **In-decoder text-level dedup** inside `pancetta-ft8`'s decode
   pipeline — a `HashSet<String>` over the decoded `Ft8Message::
   to_string()` value across all decoder passes in one slot. This
   already deduplicates if pancetta runs the decoder multiple times
   on the same audio (e.g. multi-pass SIC). It is structurally
   equivalent to WSJT-Z's mechanism for the case of multiple
   passes operating on the *same* audio buffer.

2. **Coordinator-level emission filtering** — the coordinator
   pipeline (`pancetta/src/coordinator/pipeline.rs`) currently
   takes the union of all decoder pass outputs and ships them
   through to the autonomous operator. There is no sub-slot
   re-decode at 11.8 / 13.5 / 14.7 s; pancetta decodes once
   per slot at the end.

So WSJT-Z's mechanism *as defined* does not slot in directly,
because pancetta is not currently running the three sub-slot
decodes. If pancetta were to adopt the WSJT-X / WSJT-Z multi-
trigger pipeline (which would shave ~3 s off the time-to-decode
for early signals — see "implementation notes" below), then the
HashSet dedup already in pancetta-ft8 would handle the same job
that `allmessages` does in WSJT-Z, provided the HashSet is
scoped to the slot (cleared at slot boundary) rather than to a
single decoder invocation. The key adaptation would be promoting
the HashSet from "per `decode()` call" to "per slot, persisting
across the three sub-slot calls".

The mechanism also interacts well with hb-091 (scoped fast-path):
the early 11.8 s pass is exactly the regime where a narrow-band
scoped decode would be cheap enough to actually fit inside a 200 ms
budget, making this dedup mechanism the natural enabler of low-
latency reporting for the QSO-partner band.

## Estimated Rust port effort

- ~50 LOC for the dedup data structure: a `SlotDedup` struct
  holding `HashSet<String>` plus a `slot_start: SystemTime`, with
  `try_insert(msg_text) -> bool` returning false on duplicate.
- ~30 LOC to wire it into the coordinator's per-slot processing
  loop so that the slot boundary clears the set.
- ~100 LOC if we *also* adopt the three-trigger sub-slot decoder
  pipeline (the larger architecture change WSJT-X uses) — gating
  decoder invocations on `nzhsym`-equivalent audio buffer fill
  level, triggering a partial decode at the 11.8 s / 13.5 s /
  14.7 s marks. This is plan-sized on its own.
- 1 implementation session for the dedup alone.
- 2-3 sessions for the full sub-slot triggering, mostly because
  it requires audio-pipeline changes around how the coordinator
  hands buffers to the FT8 decoder mid-slot.

Total for the dedup mechanism alone: ~80 LOC, 1 session.
Total including the sub-slot triggering: ~250 LOC, 3 sessions.

## Implementation notes for the implementer thread

- The dedup mechanism is the easy half. Drop a `HashSet<String>`
  (or `HashSet<[u8; 37]>` if we want to mirror the fixed-width
  Fortran layout) into the coordinator's per-slot state. Each
  decode hand-off checks `set.insert(msg_text)`; the bool return
  tells you whether to emit or drop.
- The size cap (200) is essentially a safety valve. Rust's
  `HashSet` doesn't need it for correctness; it's safe to omit
  the cap and rely on natural memory bounds. A 1000-entry cap
  would be a reasonable belt-and-braces choice.
- The slot reset happens on slot rollover. In pancetta the slot
  boundary is detected in `coordinator/pipeline.rs` (search for
  the 15 s tick). Clear the set there. Do NOT clear inside the
  decoder; the decoder may be called multiple times per slot.
- **Critical**: the dedup must be applied at the *coordinator*
  layer (after FT8 decode, before handing off to the autonomous
  operator and the logger), not inside the FT8 decoder. The
  decoder's existing HashSet is per-invocation; the new one
  must span all sub-slot decodes within a slot.
- The whole win of this mechanism in WSJT-Z is *latency*:
  reporting at 11.8 s instead of 14.7 s. If pancetta only runs
  one decode per slot (at the slot's end), the dedup is a no-op.
  Make sure to land the sub-slot triggering at the same time, or
  to clearly mark the dedup as preparation for a future sub-slot
  pipeline. Don't ship the dedup as a standalone "win" — it has
  no value without the pipeline change behind it.
- The pancetta-side interaction with hb-091 is interesting: hb-091
  collapses the search band when a QSO partner is locked. In the
  same regime, an 11.8 s early decode of the QSO partner's
  response is highly desirable (cuts QSO turnaround by ~3 s on
  average). The sub-slot trigger + dedup combination is a
  natural follow-on to hb-091, not a competitor.
- Pancetta's QSO state machine already has a 60 s "recently
  responded to" suppression list for the autonomous operator. The
  WSJT-Z mechanism complements that — the 60 s list prevents
  re-responding to the same callsign across slots; the WSJT-Z
  mechanism prevents the *decoder pipeline* from emitting the
  same text three times within a slot. They are orthogonal, both
  needed for clean automation behaviour, neither subsumes the
  other.
- Exact comparison is fine. Pancetta's `Ft8Message::to_string()`
  is deterministic from the payload bits (per the project memory
  on hb-092 codeword-dedup), so two decoded messages of the same
  underlying payload will produce byte-equal text. No
  normalisation needed.
