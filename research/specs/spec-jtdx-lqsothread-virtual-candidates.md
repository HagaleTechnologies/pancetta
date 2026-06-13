# Algorithm spec: JTDX lqsothread virtual candidate injection

## Source attribution
- Origin: JTDX (https://github.com/jtdx-project/jtdx)
- File paths (for traceability, NOT to be quoted):
  - `lib/sync8.f90` (virtual candidate injection at the end of the
    candidate list)
  - `lib/ft8_decode.f90` (where `lqsothread` is computed and passed in)
  - `lib/ft8b.f90` (where the virtual candidates are consumed: the
    `iqso=3` / `xdt < -4.9` and `xdt > 4.9` branches)
  - `lib/ft8s.f90` (the QSO-context "super decoder" that is run on
    virtual candidates when present)
- License: GPL-3.0
- Reader date: 2026-06-08

## Purpose

In JTDX's pass loop, when the operator is engaged in a QSO with a
specific partner (`hiscall`), the QSO partner's audio frequency
(`nfqso`) is known and steady, but the partner's actual `xdt`
(time-offset) can drift to the edges of the slot — either because the
partner started transmitting late or because their clock or path-delay
drifted. The standard sync detector clips the DT search window at
`±2.5 s` (normal) or `±3.5 s` (SWL), so a partner whose true DT is at
`+5 s` or `-5 s` will never produce a `sync8` candidate strong enough
to be picked up. The "lqsothread virtual candidate" mechanism unblocks
this: at the end of `sync8`, JTDX appends two fake candidates at
`(nfqso, +5 s)` and `(nfqso, -5 s)` with sync=0, then in `ft8b` runs a
template-matched (QSO-context-aware) super-decoder against them. The
fix is small in code and only fires when a QSO is genuinely active, so
it pays off without inflating FP risk elsewhere in the band.

This is complementary to pancetta's hb-217 "force-emit on RR73 token"
fix: both mechanisms are *force-emit family members* that synthesize
a decode the rest of the pipeline can't reach. hb-217 force-emits a
text token that the parser already declined; the JTDX mechanism
force-emits a candidate slot that the sync scorer never even tried.

## Algorithm description (PROSE ONLY)

### Inputs (in `sync8`)

- Search band `[nfa, nfb]` and QSO partner audio frequency `nfqso`.
- A flag `lqsothread` set true by the caller whenever `nfqso` is
  inside `[nfa, nfb]` for *this* call to `sync8`. (See `decoder.f90`:
  it splits the audio band into 1..5 sub-bands per pass, so
  `lqsothread` is only true for the sub-band containing the partner.)
- The candidate buffer being assembled (sorted by sync, prioritised
  near `nfqso`).

### Inputs (in `ft8b`)

- A candidate `(f1, xdt, sync)` from the buffer that may be a
  virtual one (sync = 0, xdt = ±5 s, freq exactly = nfqso).
- The current QSO context (`hiscall`, `nlasttx`, `lastrxmsg`,
  `levenint`/`loddint`, `calldteven`/`calldtodd`).

### Outputs

- Up to two extra candidate slots inserted at indices
  `k = ncandfqso+1, ncandfqso+2` of the per-pass candidate buffer.
  Both are placed *before* the body of frequency-sorted candidates so
  the inner decoder visits them early in the per-pass loop.
- When the inner decoder reaches a virtual candidate, it routes it
  into a QSO-context "super decoder" (the `ft8s` / `ft8sd1` /
  `ft8mf1` / `ft8mfcq` family) and skips the costly LDPC + AP
  cascade.

### Steps — virtual candidate injection (in `sync8`)

After the normal candidate buffer has been sorted by sync (with
optional DT weighting from the `ncandthin` mechanism) and the
near-`nfqso` candidates have been promoted to the head of the list,
JTDX checks the flag `lqsothread`. When true:

1. Write `candidate(k) = (freq=nfqso, xdt=+5.0, sync=0.0, cqflag=0)`
   and increment `k` and `ncandfqso`.
2. Write `candidate(k) = (freq=nfqso, xdt=-5.0, sync=0.0, cqflag=0)`
   and increment `k` and `ncandfqso`.
3. Continue with the body of frequency-sorted real candidates
   afterwards, still capped at 460.

The virtual candidates carry sync=0, which would normally fail every
acceptance threshold. They are not filtered later — the cap and
near-`nfqso` exemption let them through unchanged. The `cqflag` is
zero because the virtual candidates are not CQ-pattern-tagged.

### Steps — virtual candidate consumption (in `ft8b`)

The outer `ft8b` body iterates a `do iqso=1,nqso` loop over up to four
"QSO modes":

- `iqso=1`: normal decode of this candidate.
- `iqso=2`: virtual2 — partner's true DT is the previously latched
  `lastrxmsg(1)%xdt`; cd0 is replaced by `cd2` (or `cd3` for
  lvirtual3, which is the second virtual lane).
- `iqso=3`: virtual3 — symmetric for the negative-DT case.
- `iqso=4`: an FT8SD shortcut decode against a candidate text held in
  `evencopy` / `oddcopy` (already-seen messages on this interval).

The virtual-candidate path is gated:

1. Early in `ft8b`, when `lqsothread` is true and the candidate has
   `|xdt| > 4.9 s` *and* `|f1 - nfqso| < 0.1 Hz`, JTDX considers
   activating one of the virtual lanes (`lvirtual2` or `lvirtual3`).
2. If `lastrxmsg(1)%lstate` is true (we already have a "last received
   message" from the partner), the virtual lane reuses
   `lastrxmsg(1)%xdt` as the assumed DT — that means the algorithm
   *believes the partner's DT has drifted* from where the sync scorer
   would have found it, and is using the operator's own QSO memory of
   where the partner has been transmitting from.
3. If `lastrxmsg(1)%lstate` is false, JTDX walks back through the
   `calldteven` / `calldtodd` rolling arrays (the last ~150 callsigns
   decoded on this parity with their DTs) looking for `hiscall`. If
   it finds it, it uses that historical DT.
4. The virtual lane sets `nqso` to 2 or 3 so the outer loop reaches
   `iqso=2` or `iqso=3` and re-downsamples with the adjusted DT.
5. Inside the virtual lane, the inner decoder is **not** the full
   subpass cascade. It calls `ft8s` (the JTDX-specific
   "super-decoder for QSO partner") which:
   - Knows the set of plausible messages the partner could have sent
     given the operator's last TX (`nlasttx` ∈ {1..5}).
   - Performs a template match against the 25 candidate text strings
     in `msg(...)` with pre-encoded tone sequences `itone56`.
   - Returns success only if the tone-match score exceeds a
     low-but-context-dependent threshold.

### Numerical constants (facts, not expression)

- Virtual candidate DT offsets: `+5.0 s` and `-5.0 s` (in slot-relative
  units, i.e. relative to the nominal 0.5 s TX start; absolute DT is
  `xdt - 0.5` after the candidate exits `sync8`).
- Virtual candidate sync score: `0.0`.
- Virtual candidate frequency: exactly `float(nfqso)`.
- Frequency proximity gate to activate virtual lane in `ft8b`:
  `|f1 - nfqso| < 0.1 Hz`. The condition `abs(f10-nfqso).lt.0.1` only
  triggers when the candidate is the manufactured one at
  `freq = nfqso`.
- Activation gate on `xdt`: `xdt > +4.9 s` (for `lvirtual2`) or
  `xdt < -4.9 s` (for `lvirtual3`). Any real candidate would have
  been rejected before reaching `ft8b` because the sync DT window is
  `[-49, +76]` spec units (`[-2.0, +3.0] s`) or `[-74, +101]`
  (`[-3.0, +4.0] s`) for SWL. The two virtual offsets are *outside*
  the sync DT window by design.
- Activation gate on previous TX: `nlasttx ∈ {1..4}` (the operator
  must be in mid-QSO; not transmitting CQ, not halted). `maxlasttx`
  bumps to 5 in the special case where the partner's last received
  message was a `RRR` exchange.
- Callsign-history search depth: 150 entries in `calldteven` /
  `calldtodd` (one per parity-rotation; effectively the last 75
  minutes of decoded callsigns at standard 15 s cadence).
- Gate on partner-callsign known: `len_trim(hiscall) > 2` (callsign
  must be at least 3 characters).
- Hound-mode sensitivity gate: when `nft8rxfsens < 3` and the inner
  loop reaches `iqso=3` (virtual3), it is skipped; only virtual2 fires.

### Edge cases

- `lqsothread` is only set in `sync8` if the caller's sub-band
  contains `nfqso`. In multi-decoder configurations
  (`numthreads > 1`), the audio band is split into halves, thirds, or
  fifths; the virtual candidates are appended at most once across the
  whole pass cycle.
- When `lqsothread` is true but the partner's callsign is empty
  (`hiscall == ''`), the virtual candidate is still appended in
  `sync8`, but it is dead — `ft8b` only activates the virtual lane
  when `len_trim(hiscall) > 2`.
- When `lastrxmsg(1)%lstate` is true and the message ends with
  `RR73`, the `maxlasttx` gate widens to 5, meaning even an operator
  who has already TX'd a `73` confirmation will still allow the
  virtual lane to fire (the partner may be slowly re-confirming).
- `lft8sdec` (the "I've already QSO-super-decoded this slot") flag
  blocks the virtual lane from firing twice in the same slot.
- The virtual candidates do not interact with subtract-and-re-decode
  (SIC). They live entirely inside the per-pass candidate iteration
  and the inner `ft8s` routine handles them on the un-subtracted
  downsampled buffer.
- The two virtual candidates appear *adjacent* in the list (k and
  k+1, both with frequency = nfqso); pancetta's existing candidate-
  level dedup that operates on `(freq, dt)` pairs must not collapse
  them. JTDX's `sync8` dedup window is 4 Hz / 0.1 s and explicitly
  exempts the near-`nfqso` zone, so the virtual pair survives.

## Conflict with pancetta's existing mechanisms

Pancetta has hb-217 (the RR73 force-emit fix) which lives in
`pancetta-ft8/src/message.rs` around line 1407 — it synthesises an
RR73 token when the grid-shaped filter has consumed it. That is a
*token-level* force-emit. The JTDX virtual candidate is a
*candidate-level* force-emit: it inserts a synthetic sync candidate
ahead of the real ones and lets the QSO-context decoder run on it.

Compatibility:

1. Pancetta does not currently have a QSO-context aware "super
   decoder" that knows the set of plausible partner messages given
   `nlasttx`. The closest analogue is `pancetta-qso`'s
   `autonomous.rs` plus the `last_rx_msg` tracking on the active QSO
   state. A port of the JTDX mechanism would need both:
   - A *candidate-injection* hook in pancetta-ft8's coarse-search
     output (the place where pancetta currently emits `(freq, dt,
     sync)` tuples for the message decoder).
   - A *QSO-context decoder* in pancetta-ft8 that takes a `(nfqso,
     xdt, expected_message_template)` triple and returns a candidate
     decode. This is approximately a tone-template matched filter
     scoring 7 (or 8) tone positions against the expected tone
     sequence.
2. Pancetta's slot-edge hole is the *primary* miss mechanism for the
   strong-signal coverage drop at negative-DT (slot-edge at 48.3 %
   recall in hb-218 audit, see MEMORY.md). The lqsothread virtual
   candidates *directly* attack the negative-DT side of that hole —
   but only for the operator's QSO partner, not for the band at
   large. So this is a precision win on a narrow target, not a recall
   win across the band.
3. The bonus interaction with the slot-edge hole is the most
   strategic: any QSO partner whose DT has drifted into the
   slot-edge zone is currently invisible to pancetta. Wiring this
   mechanism into pancetta-qso's *active QSO state machine* (so it
   only fires when an active QSO exists) would close the partner-
   visible portion of the slot-edge hole with bounded FP risk.

## Estimated Rust port effort

- ~80 LOC to inject the two virtual candidates at the end of
  pancetta-ft8's coarse-search candidate list, gated by
  `qso_state.active_partner.is_some()`.
- ~250 LOC for a QSO-context tone-template matched filter that, given
  a partner callsign and the operator's last TX, scores the 56
  pre-encoded message templates against the symbol-magnitudes at
  `(nfqso, ±5 s)`. Reuses `pancetta-ft8::encoder::encode_message_77`
  to pre-compute templates per QSO partner.
- ~100 LOC of plumbing in pancetta-qso to pass `(active_partner_call,
  last_tx_kind, last_rx_msg_xdt)` into the FT8 decode call.
- ~50 LOC of tests: round-trip a partner who TX'd at +5 s DT through
  a synthetic WAV and confirm the virtual lane picks it up.
- ~150 LOC of a calibration example to set the tone-match threshold
  (use a noise-only WAV at nfqso, and a known-signal WAV at
  nfqso/+5 s DT, walk the threshold across an SNR sweep).
- 2 research sessions: one to gather noise-only + synthetic-signal
  data for threshold calibration, one to wire and ship.

Total: ~630 LOC, 2-3 sessions.

## Implementation notes for the implementer thread

- The injection point in pancetta is at the *end* of the coarse-search
  emit loop, after the normal `(freq, dt, sync)` candidates have been
  added to the buffer. Add them only if the active QSO state's
  partner is known and the partner's audio frequency lies in the
  current decode band.
- The injected candidates should carry a marker (e.g. an enum variant
  `Candidate::VirtualQsoPartner { nfqso, dt: f32 }`) so the inner
  decoder can route them to the QSO-context decoder instead of the
  normal Costas refinement + bp_decode + OSD cascade.
- The QSO-context decoder should *not* invoke OSD. OSD's strength is
  generic AP — JTDX deliberately uses a template-match (FT8S) here
  because the message space is small (~25-56 candidates depending on
  QSO state) and a template-match is both faster and lower-FP than
  OSD with AP bits.
- pancetta's QSO state machine in `pancetta-qso/src/qso_manager.rs`
  already tracks `last_rx_msg`, `tx_parity`, and `active_partner`.
  Thread these into the decode call via the coordinator's pipeline
  configuration.
- Cross-reference hb-217: the RR73 force-emit fix lives at
  `pancetta-ft8/src/message.rs:1407` and is the right model for
  conservative force-emit. The JTDX virtual-candidate mechanism is a
  larger surface area but the same philosophy: synthesize the
  candidate the rest of the pipeline can't reach, gate it tightly,
  let downstream dedup discard if it's wrong.
- Initial FP risk profile: very low. The mechanism only fires when an
  active QSO state exists, only synthesizes candidates at the partner's
  exact audio frequency, and only matches against the small message
  template set. The dominant FP mode is "partner is silent and a
  noise-only window matches one template by chance" — calibrate the
  tone-match threshold against a noise-only corpus.
- Order with respect to `spec-jtdx-relaxed-sync-near-partner.md`: the
  relaxed sync (1.1 threshold within ±3 Hz of nfqso) handles partners
  whose DT is *in band*. The lqsothread virtual candidates handle
  partners whose DT is *out of band*. Both are partner-aware
  mechanisms; ship them together for best coverage.
