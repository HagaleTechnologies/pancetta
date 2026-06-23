# Algorithm spec: a8 — early sequenced-QSO a-priori decoding

## Source attribution
- Origin: WSJT-X Improved by Uwe Risse, DG2YCB
  (https://sourceforge.net/projects/wsjt-x-improved/)
- Primary documents (traceability only, NOT quoted):
  `Release_Notes.txt` lines ~152–161 (v3.0.0 250924), lines ~202–207
  (v3.0.0 251101), line 85 (v3.1.0 260522 — new "skip a8" option for MTD).
- Fortran source paths cited for traceability only, NOT to be read by implementer:
  `wsjtx/lib/ft8/ft8_a8d.f90` (a8 decode core),
  `wsjtx/lib/ft8/ft8_a8_test.f90` (a8 candidate validator),
  `wsjtx/lib/ft8/ft8_a7.f90` (the AP-7 cousin already extracted as
  spec-wsjtr-cross-sequence-a7.md),
  `wsjtx/lib/ft8/ft8apset.f90` (AP message-template construction),
  `wsjtx/lib/ft8/ft8b.f90` and `wsjtx/lib/ft8/ft8d.f90` (single-threaded
  decoder driver / multipass loop),
  `wsjtx/lib/ft8_decode.f90` (top-level FT8 decode dispatcher).
- License: GPL-3.0.
- Reader date: 2026-06-08.
- Reader thread: clean-room extraction (prose only).
- Upstream lineage: a8 is exclusive to WSJT-X Improved as of v3.0.0; it
  is not present in WSJT-X mainline. v3.0.0 (251101) shipped the MTD
  version of a8; v3.1.0 (260522) added an option to skip a8 in MTD for
  operators who do not want it.

## Purpose

FT8 QSOs are an alternating sequence of 15-second transmissions: station
A in even-parity slots, station B in odd-parity slots. From the moment
the decoder commits to a particular DX station as the QSO partner (by
sending a reply, or by the operator clicking the partner's callsign), the
*set of legal next messages* from that partner collapses dramatically.
For example, if you've just sent "DX K1JT R-12", the next message K1JT
will send is almost certainly "K1JT DG2YCB RRR" or "K1JT DG2YCB RR73" or
"K1JT DG2YCB 73", at the same frequency as her previous transmission.
The candidate space is on the order of ~10 messages, not 2^77.

The a8 decoder uses this. It is a high-priority AP pass that runs **early
in the receive window**, before the multipass loop has converged, attempting
to decode the QSO partner's signal at her known frequency against the small
hand-picked candidate set of legal next messages in the current QSO state.

When it succeeds, the partner's message is displayed 0.5 to 1 second
earlier than it would have been by the standard pipeline. The operational
benefit is faster QSO turnaround: the next Tx slot can be queued sooner
because the partner's reply was decoded sooner.

a8 is structurally adjacent to the cross-sequence a7 mechanism (see
spec-wsjtr-cross-sequence-a7.md) but differs in two important ways:

- **Timing**: a7 runs *after* the multipass loop has finished, as a
  post-cleanup. a8 runs *before* the multipass loop completes, racing
  to beat normal decoding to the punch.
- **Seed source**: a7 seeds from the entire opposite-parity previous
  window's decode table (any callsign-pair that was decoded). a8 seeds
  specifically from the current operator-active-QSO context — only
  callsigns the operator is in an active QSO with.

## Algorithm description (PROSE ONLY — no code)

### Inputs

The algorithm operates on a per-window basis. Inputs at each invocation:

- **A partial audio buffer for the current window.** The decoder begins
  populating its FFT input buffer as audio arrives; a8 fires at a buffer
  fill that is *earlier* than the full 15-second mark. (The single-threaded
  decoder is structured around an `nzhsym` parameter that counts half-symbol
  units of audio collected so far; values commonly used in WSJT-X are 41,
  46, 48, 49, 50 — corresponding to roughly 11.8s, 13.2s, 13.8s, 14.1s,
  14.4s of audio. a8 conceptually runs against an early `nzhsym` value so
  the answer is ready before the full-frame final pass.)
- **Active QSO state**, conceptually a small struct per outstanding QSO:
  - `dx_call`: the partner's callsign.
  - `my_call`: the operator's callsign.
  - `dx_freq_hz`: the frequency at which the partner's most-recent
    transmission was decoded (or expected — e.g., reply-to-CQ
    convention may be at the responder's Tx frequency).
  - `qso_state`: which sub-step of the QSO sequence is the operator in?
    States, in canonical order, are: `awaiting-reply-to-cq`,
    `replied-with-grid`, `replied-with-report`, `acknowledged-with-r-report`,
    `sent-rrr-or-rr73-or-73`. Each state implies a small set of legal
    next messages from the partner.
  - `grid4` (optional): the partner's grid square if known.
  - `report` (optional): the signal report the operator just sent (so the
    R-prefixed reply variant can be enumerated).
- **`enable_ap` operator flag**. If false, a8 is disabled (consistent with
  a7's same gating; release-notes line ~1303 documents that 'a7' decodes
  are disabled when "Enable AP" is unchecked, and we treat 'a8' the same).
- **MTD-skip operator flag** (`skip_a8_in_mtd`, new in v3.1.0). If set,
  the multithreaded-decoder variant of a8 is skipped; the single-threaded
  variant is independently controlled by `enable_ap`. This is a
  user-preference dial, not an algorithmic gate.

### Outputs

A list of decodes attributed to a8. Each decode includes the standard
DecodedMessage fields plus an AP-provenance tag (in WSJT-X output the
flag character is `a8`; in pancetta the equivalent is a `DecodeProvenance`
enum value). The same release-notes entry references "very low SNR
values for 'a8' decodes" being inconsistent in early builds — this
implies a8 reports an SNR estimate just like normal decodes, computed
post-acceptance from the matched waveform.

### Steps

1. **Trigger condition.** Before each pre-final decode pass, examine the
   active QSO state. If there is at least one outstanding QSO (a `dx_call`
   has been latched and not yet logged-or-aborted), and AP is enabled, run
   the a8 pass for each outstanding QSO before the next standard decode
   pass kicks off. In WSJT-X Improved this fires at the early `nzhsym`
   boundary used by the MTD scheduler.

2. **Per-QSO candidate enumeration.** Given `(dx_call, my_call, qso_state,
   grid4, report)`, enumerate the small set of plausible next messages
   the partner would send. The enumeration is gated on QSO state:
   - `awaiting-reply-to-cq` (operator called the DX after a CQ): the
     partner's expected reply is `dx_call my_call <grid4|R-report>`.
     Enumerate the grid variant and ~3 standard report variants
     (e.g., 0, -10, -20 if no specific report is known, OR the actual
     report just sent if the operator's previous Tx already included one).
   - `replied-with-grid` (operator sent `dx_call my_call grid4`): the
     partner's expected reply is `dx_call my_call -NN` or
     `dx_call my_call R-NN`. Enumerate the ~10 most-likely report values
     (e.g., snrs near what was sent in the operator's own report, or a
     small range like -20..0).
   - `replied-with-report` (operator sent `dx_call my_call R-NN`): the
     partner's expected reply is `dx_call my_call RR73` or
     `dx_call my_call RRR` or `dx_call my_call 73`. Enumerate ~3
     candidates.
   - `acknowledged-with-r-report` (operator sent `dx_call my_call RR73`):
     the partner's expected reply is `dx_call my_call 73` or no message
     at all. Enumerate ~1–2 candidates.
   In each ordering only one ordering is used here — the partner's
   message has `dx_call` first (the partner is addressing the operator).
   The cross-sequence a7 enumeration tries both orderings; a8 does not,
   because the QSO context fixes the addressing direction.
3. **Per-candidate codeword.** For each candidate text:
   - Pack into the 77-bit FT8 payload via the standard packer.
   - LDPC-encode to 174 bits via the standard encoder.
   - This produces an exact expected bit pattern (the "AP truth").

4. **Frequency-locked fine sync at the partner's frequency.** At
   `dx_freq_hz` ± a small refinement window (analogous to the cross-sequence
   a7 fine-sync windows of ±2.5 Hz coarse / ±0.5 Hz refine), sync against
   the Costas arrays in the partial audio buffer. Reject the seed if
   no plausible sync candidate exists.

5. **Soft-symbol extraction at locked sync.** Extract per-symbol soft
   metrics (LLRs) for the 174 bit positions, using the same demodulation
   primitives the standard decoder uses (the same `ft8d.f90`-style
   per-symbol coherent metric, fed by the same downsampled baseband).
   The set of LLR variants is the same amplitude-domain pair used by
   a7 (single-symbol coherent vs. block coherent × per-normalization);
   no power-domain variants.

6. **Per-candidate weighted distance scoring.** For each candidate:
   - Compute the weighted Hamming distance between the candidate's
     LDPC-encoded 174-bit truth and the soft LLRs: for every bit, if
     the LLR's sign disagrees with the candidate bit, add `|LLR|` to
     a running sum; if it agrees, add nothing.
   - Take the minimum across LLR variants.
   - Track first-best `dmin` and second-best `dmin2` across all
     candidates for this seed.

7. **Acceptance gate.** Same family as a7: a candidate is accepted iff
   both:
   - `dmin <= ceiling` (a7 uses 100.0; a8 may use a tighter bound
     given that the AP is even stronger — call it `dmin_a8` ≤ ~90,
     subject to empirical tuning).
   - `dmin2 / dmin >= margin` (a7 uses 1.3; a8 likely the same or
     tighter, say 1.4).
   Reject otherwise.

8. **CRC verification.** Run the FT8 CRC-14 check on the accepted
   candidate's 91-bit payload. (This is implicit in the LDPC pipeline
   for normal decodes, but for AP decodes it is a separate explicit
   gate — the candidate text was assumed, not solved-for, so the CRC
   gives a clean cross-check.) Reject on CRC failure.

9. **Emit with provenance tag.** Emit the decoded message with the AP
   provenance flag (`a8` in WSJT-X output). Record SNR using the same
   formula as normal decodes (matched-waveform energy / noise floor at
   the sync point).

10. **Subtract from residual.** Same as any decoded signal — subtract
    the reconstructed time-domain waveform from the audio buffer so that
    the remainder of the multipass loop sees a cleaner residual. (In
    WSJT-X this is the `subtractft8` step.) Importantly, since a8 fires
    early, this subtraction also benefits the later standard passes that
    haven't yet run.

11. **Race outcome.** When the standard final pass later decodes the
    same signal, the duplicate-suppression logic (already present in
    WSJT-X for every multipass stage) discards the redundant decode.
    Net effect: the partner's message text appears on screen at the
    a8-pass timing — 0.5 to 1 second sooner than the standard final
    pass would have shown it.

### Numerical constants (facts, not expression)

- **Early-display benefit**: a8 surfaces partner-callsign decodes
  **0.5 to 1.0 seconds earlier** than the standard pipeline (verbatim
  from release notes; this is the user-observable upper bound under
  good conditions).
- **`nzhsym` family** (single-threaded decoder timing — the parameter
  that controls how much audio has been collected before a pass fires):
  41 ≈ 11.8s, 46 ≈ 13.2s, 48 ≈ 13.8s, 49 ≈ 14.1s, 50 ≈ 14.4s. a8
  fires at an early `nzhsym` (well before 50). Exact value not in
  release notes; pancetta should pick the earliest value at which the
  audio buffer contains a complete FT8 frame at the seed sync offset.
- **Acceptance gate ceiling**: a7 uses `dmin ≤ 100.0` (weighted-Hamming
  LLR units). a8's tighter analog is at the implementer's discretion;
  start at 100.0 to match a7 and tighten only if FPs surface.
- **Acceptance gate margin**: a7 uses `dmin2/dmin ≥ 1.3`. a8 likely
  the same or tighter.
- **Candidate count per seed**: small (typically ≤10 per active QSO,
  determined by the qso_state branching above). Far smaller than a7's
  ~206 — that is the source of a8's CPU efficiency.
- **Frequency window**: same as a7 — ±2.5 Hz coarse / ±0.5 Hz refine
  around `dx_freq_hz`. Time window: ±10 samples (12 kHz sample rate
  → ~±0.8 ms).
- **MTD-skip flag**: per release notes, v3.1.0 adds an explicit "skip
  a8 in MTD" toggle. Implementers should mirror this with a
  `enable_a8: bool` (Fast/Moderate tier default `true`, Slow tier
  default `false`).

### Edge cases

- **No active QSO.** Skip a8 entirely. This is the common case at the
  start of a session, on band edges, and between QSOs.
- **Multiple simultaneous QSOs (multi-stream Tx).** Pancetta supports
  N simultaneous QSOs each at a distinct audio frequency. The a8
  pass must iterate per QSO. Each QSO carries its own
  `(dx_call, dx_freq_hz, qso_state)` and is evaluated independently.
  Total cost scales linearly in active-QSO count.
- **Partner has not yet transmitted in this QSO.** If `dx_freq_hz` is
  unset (the operator clicked the DX from band-activity but the DX has
  not been heard yet at a confirmed frequency), a8 has nothing to seed
  off and must skip.
- **Partner has changed frequency mid-QSO.** Rare but legal; a8 will
  fail to sync and the seed is rejected. The standard pipeline picks up
  the new frequency in the regular passes. No special handling needed.
- **CRC accepts a wrong candidate.** With LDPC + CRC-14 even an AP-shoved
  wrong-text candidate is extremely unlikely to pass — the CRC space
  is 16384× larger than the candidate set. Belt-and-braces nonetheless:
  the `dmin / dmin2` margin gate catches it first.
- **`enable_ap` disabled by operator.** a8 is suppressed wholesale.
  Behavior reverts to standard pipeline timing. Per release-notes
  line ~1303, the same gating applies to a7.
- **`skip_a8_in_mtd` set.** The MTD variant of a8 is suppressed; the
  STD variant is still allowed (or independently gated by `enable_ap`).
- **Inconsistent SNR for very low signals.** Per release notes, v3.0.0
  (250924) corrected an inconsistency in a8's SNR reporting at very low
  signal levels. Implementers should validate SNR computation across
  a range of input SNRs, especially the −20 dB to −24 dB regime where
  the matched-waveform energy estimator is most fragile.

## Conflict with pancetta's existing mechanisms

- **Pancetta has no AP pre-pass infrastructure.** The current decoder
  does not maintain per-QSO state inside the FT8 hot loop. Adding a8
  requires (a) plumbing active-QSO state into the decoder context (the
  QSO manager already maintains it — see
  `pancetta-qso/src/qso_manager.rs`) and (b) wiring an "early pass"
  hook into the decoder before the multipass loop completes.

- **Strong synergy with planned a7 (cross-sequence) work.** Both
  mechanisms share the same primitives: per-seed baseband extraction,
  fine-sync, soft-symbol extraction, weighted-Hamming distance against a
  small candidate set, and the same acceptance gate. The recommended
  implementation order is a7 first (cleanup pass; lower risk because
  it runs after the standard pipeline) then a8 (early pass; benefits
  from a7's verified primitives).

- **Conflict with pancetta's current synchronous decode model.** Pancetta
  processes one full 15-second window at a time. a8's value depends on
  decoding *before* the window is fully ingested — that is a flow change.
  Two implementation options:
  1. Cheap option: run a8 at the end of the window but flag the decode
     with a "would-have-been-early" provenance. This drops the
     operational benefit but provides a clean upgrade path.
  2. Real option: refactor the decoder to accept a partial-buffer
     input and add an "early pass" entry point. This is non-trivial; see
     effort estimate.

- **Interaction with hb-217 (RR73 fix).** Strongly positive. a8's
  acknowledgment-state enumeration relies on RR73 / RRR / 73 candidates
  being valid; pancetta's pre-hb-217 RR73 emission rate was ~150× worse
  than jt9. Without hb-217 a8 would mostly fail at the final QSO step.
  With hb-217 in place, a8 can complete QSOs end-to-end.

- **Interaction with hb-103 (content score).** a8 decodes are
  high-confidence by construction (LDPC + CRC + AP-seeded). Pancetta's
  content score should special-case `DecodeProvenance::A8` and
  unconditionally accept (or accept at the most lenient threshold)
  rather than running the normal feature-based scoring. Otherwise a8
  decodes risk being filtered out at the autonomous-TX gate.

- **Interaction with hb-091 (scoped fast path) and hb-216 (tier
  classification).** On the Slow tier, a8 should default to
  `enable_a8: false`. Both because the early-pass timing budget on a
  slow host is tight, and because the per-QSO state plumbing is itself
  CPU work that a Slow host should skip. Fast and Moderate tiers
  default to `true`.

- **Interaction with hb-058 / hb-062 (FP filters).** a8's accepted
  decodes should *bypass* the FP-pattern filters (they are high-confidence
  by design and the FP filters were designed for unconstrained
  decodes). Add a `provenance: DecodeProvenance` field to
  DecodedMessage and short-circuit the filter chain when
  provenance is `A8`.

## Estimated Rust port effort

- New module:
  - `pancetta-qso/src/active_qso_ap.rs`: builds per-QSO AP candidate
    sets given `(dx_call, my_call, qso_state, grid4, report)`. Uses the
    existing QSO state machine vocabulary. ~150 LOC + ~80 LOC tests.
  - Extension to `pancetta-ft8/src/decoder.rs` (or wherever the
    multipass loop lives): new "early AP pass" entry point that takes
    a candidate-text iterator and a sync seed, runs fine-sync +
    weighted-distance + CRC, and returns accepted decodes.
    ~120 LOC + ~80 LOC tests.
  - Provenance field on DecodedMessage + plumbing through pipeline:
    ~50 LOC + 20 LOC tests.
- Coordinator wiring:
  - Active-QSO state snapshot passed into the decode call. Decoder runs
    a8 first, subtracts, then proceeds with the standard pipeline.
    ~80 LOC + ~50 LOC tests.
- Hard-200 eval + bootstrap-CI session (no code, just measure).
- Total: ~400 LOC + ~230 LOC tests.
- 5–7 iter sessions:
  1. Provenance enum + DecodedMessage field.
  2. Active-QSO AP candidate enumeration in pancetta-qso.
  3. Decoder early-pass entry point.
  4. Coordinator wiring + tier gating.
  5. Hard-200 eval (yield-rate change, FP-rate change).
  6. Bootstrap-CI bracket; ship / shelve decision.
  7. Operator field testing (Phase 5 prerequisite).

## Implementation notes for the implementer thread

- **Order of work.** Ship the cross-sequence a7 work first (see
  spec-wsjtr-cross-sequence-a7.md). Its primitives — fine-sync,
  soft-symbol LLRs, weighted-Hamming distance, the `dmin/dmin2` gate —
  are the same primitives a8 needs. Build a8 on top, do not duplicate.

- **a8 is timing-driven, not algorithm-driven.** The headline benefit
  (0.5–1 s earlier display) requires running the pass while the audio
  buffer is still being filled. If pancetta's current decoder is one
  call per fully-ingested window, the headline benefit is unavailable
  and a8 collapses to "yet another high-confidence AP pass at end of
  window." That is still useful (additional decodes) but the operational
  win is lost. Plan the audio-pipeline refactor before promising the
  early-display behavior.

- **Active-QSO state lives in pancetta-qso.** The decoder should be
  stateless across windows; the QSO state machine already maintains
  per-QSO context (see `pancetta-qso/src/qso_manager.rs`). Pass a
  snapshot (a small struct, not the live state machine) into the
  decoder. The decoder reads, never writes.

- **Per-QSO-state candidate enumeration is small.** Do not try to
  generalize this to a generic "AP template generator" — it is QSO-state-
  specific by design, and that specificity is what makes a8's candidate
  set ~10× smaller than a7's. A small finite-state table mapping
  `qso_state` → enumeration function is the right shape.

- **CRC is the safety belt.** Even with LDPC + the `dmin/dmin2` gate,
  always run CRC-14 verification on the accepted candidate's payload.
  This is cheap and catches any pathological case where weighted-distance
  scoring lands on a wrong-text candidate.

- **Provenance tagging is non-optional.** Add `DecodeProvenance` enum
  with at least `Bp`, `Osd`, `Ap` (generic AP), `A7CrossSequence`,
  `A8EarlyQsoState`. Several downstream consumers (hb-103 content
  score, autonomous-TX gating, logging, PSKReporter spotting) want
  to differentiate.

- **Tier gating.** Default `enable_a8: false` on Slow, `true` on
  Moderate/Fast. Mirror the existing tier-driven `Ft8Config` rewrite
  pattern from hb-216 S2.

- **Operator override flag.** Mirror WSJT-X Improved's v3.1.0
  `skip_a8_in_mtd` toggle. Operators may want to disable a8 in noisy
  environments where the early-pass FPs could clutter the band
  activity window.

- **Test fixture.** Use the existing pancetta loopback QSO test
  infrastructure. Construct a two-window WAV: window N has the
  operator's previous Tx, window N+1 has the partner's reply. Decode
  window N normally to populate QSO state. Decode window N+1 with a8
  enabled; confirm the partner's reply is in the decode list with
  `provenance = A8EarlyQsoState`. Repeat across all 5 QSO states
  (`awaiting-reply-to-cq` through `acknowledged-with-r-report`).

- **Citation hygiene.** Cite as "inspired by WSJT-X Improved a8
  decoding (v3.0.0 251101, DG2YCB)" in the journal entry. The
  underlying primitive (sequenced-QSO-state AP) is general — Joe Taylor's
  AP papers cover the framework; a8 is the specific WSJT-X Improved
  implementation. Pancetta's implementation is independent.
