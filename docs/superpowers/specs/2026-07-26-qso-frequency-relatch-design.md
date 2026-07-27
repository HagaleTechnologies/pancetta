# QSO Frequency Relatch — Design

**Status:** Approved for planning
**Date:** 2026-07-26

## Problem

Live tonight (QSO with LU7LRP, `64043db4-5d33-4c9d-87df-b94dd50ad67b`): we called LU7LRP
at 1500 Hz. LU7LRP's own transmissions consistently landed at 937.5 Hz — a legitimate
DX operating on an independent, fixed TX offset, not RF drift. Their real signal-report
reply (`K5ARH LU7LRP -11`, decoded twice at SNR -19 and -17) was silently discarded, and
the QSO stayed stuck in `RespondingToCq`, re-transmitting the original CQ-response call
every cycle ("spamming call") instead of advancing. It only recovered when the operator
manually triggered `RespondToCaller`, which happens to relatch the QSO's tracked
frequency by hand. The same pattern shows up 3 times in the prior day's log
(2026-07-25), so this is a recurring failure mode, not a one-off.

## Root cause (two coordinated gaps, not one)

1. **Routing gate rejects on distance, not identity.** `is_message_relevant()`
   (`pancetta-qso/src/qso_manager.rs:3438-3465`) rejects any message whose frequency is
   more than `ESTABLISHED_FREQ_TOLERANCE_HZ` (100 Hz) from the QSO's latched frequency —
   even on match arms that have *already* verified `is_partner(from, target) &&
   is_us(to)` inside the arm itself. For those arms the frequency check is redundant for
   identity (callsign+direction already uniquely bind the sender) and only serves to
   drop legitimate replies from a DX transmitting on its own independent offset.

2. **Transition function has no way to relatch even if the gate let a message through.**
   `determine_state_transition()` (line 2511) does not receive the incoming message's
   decoded frequency at all — only `signal_strength`. Every transition arm constructs
   its new state by carrying forward the *old* state's `frequency` field (e.g. line
   2816-2822: `frequency: *frequency` reads from the stale `RespondingToCq` state, not
   from the reply that just arrived). Only the manual `RespondToCaller` path (which
   takes an explicit operator-supplied frequency) actually relatches today — the
   automatic decode path structurally cannot, independent of the gate.

The routing gate's silent `return false` also has no logging at any point — this bug was
only findable by cross-referencing raw decode frequencies against QSO state transitions
by hand in the log.

## Design

### 1. Relevance gate: identity-verified arms skip the distance check — for non-Hound QSOs only

The exact set of match arms in `is_message_relevant()` that already check
`Self::is_partner(from, target) && self.is_us(to)` inside the arm (every state-specific
arm once a QSO is established) is precisely these seven:
`WaitingForReport`+`ReportAck`, `WaitingForReport`+`FinalConfirmation`/`SeventyThree`,
`RespondingToCq`+`SignalReport`, `RespondingToCq`+`CqResponse`,
`SendingReport`+`ReportAck`, `SendingReport`+`FinalConfirmation`/`SeventyThree`,
`WaitingForConfirmation`+`FinalConfirmation`/`SeventyThree`. (A few other arms in the same
state family — e.g. `RespondingToCq`+`ReportAck` "skip-rung", `SendingReport`+`SignalReport`
regression — have no explicit identity-checked arm today and fall through to the generic
`_ => is_addressed_to(...)` fallback; they are **out of scope**, unchanged.)

For these seven, when **`metadata.partner_freq.is_none()`** (i.e. not a Hound/Fox QSO),
drop the distance-from-latched-frequency check entirely and replace it with a passband
sanity bound: reject only if the decoded frequency falls outside 200–2900 Hz (matching
the existing convention used for `freq_min_hz`/`freq_max_hz` elsewhere in this crate —
`frequency.rs`, `autonomous.rs`). This is a decode-garbage guard, not an identity check.

**Critical carve-out found while tracing the code: Hound/Fox mode reuses this exact
`RespondingToCq`+`SignalReport` arm with a legitimately large, by-design frequency gap**
(`is_message_relevant_hound_keys_on_partner_freq` — Hound calls low at 600 Hz, Fox replies
at 1800 Hz, and a frame at 600 Hz, the Hound's OWN offset, must still be REJECTED as not
relevant — it's almost certainly a self-decode collision in a dense pileup, not the Fox).
So the skip-the-distance-check relaxation must be gated on `metadata.partner_freq.is_none()`
— when `partner_freq` is `Some` (Hound/Fox), the **existing distance check against
`partner_freq` stays completely unchanged**, for all seven arms.

Leave the **tight gate completely unchanged** for the genuinely ambiguous
pre-identity-establishment arms:
- `CallingCq` + `CqResponse` (we don't yet know who's answering our CQ)
- `CallingCq` + `SignalReport` (bare-report answer, same ambiguity)
- the catch-all `_ => message_type.is_addressed_to(...)` fallback

These are exactly the cases the 2026-04-29 security review (catalog C-1, 50→15 Hz
tightening) targeted, and remain unchanged.

### 2. Transition function: relatch to the incoming message's real frequency — also gated non-Hound

Thread the incoming message's decoded `frequency` (`QsoMessage.frequency`, already
captured in `process_message_for_qso`) through to `determine_state_transition()` as a
new parameter (`incoming_frequency: f64`), plus one more: whether this QSO is a Hound QSO
(`progress.metadata.hound: bool`, already captured under the same write lock the
existing `qso_frequency`/`qso_tx_parity` locals are). For the exact same seven arms
listed above, construct the new state's `frequency` field as `if is_hound { *frequency }
else { incoming_frequency }` — i.e. relatch only for ordinary (non-Hound) QSOs; Hound
QSOs keep today's carried-forward value from the transition function's point of view.

This mirrors exactly what the manual `RespondToCaller` override already does by hand for
ordinary QSOs, just automatic. Hound is untouched here because it doesn't need this fix:
`process_message_for_qso` already has its own dedicated post-transition QSY block (search
`hound_qsyed`) that unconditionally overwrites `progress.metadata.frequency` — and even
reaches into the just-built `QsoState::SendingReport`'s `frequency` field directly — with
a computed response-region offset the moment a Hound's `RespondingToCq`→`SendingReport`
transition fires. Whatever `determine_state_transition` produces for a Hound QSO's
`frequency` field is clobbered by that block regardless, so leaving it at the old
carried-forward value for Hound is both safe (matches existing, already-tested behavior)
and simpler than trying to reconcile two frequency-correction mechanisms.

The **only** production call site is `process_message_for_qso`'s call to
`determine_state_transition` (`pancetta-qso/src/qso_manager.rs:2115-2122`); six existing
unit tests call it directly too and all need the two new arguments added (trivial,
mechanical — none of the six exercise the seven relatch-eligible arms in a way whose
assertions depend on the specific frequency value carried, so passing the same frequency
already in their fixture state plus `is_hound: false` preserves their behavior exactly).

### 3. Observability

Add a `debug!` (or `info!`, matching the existing `qso.security`/`qso.advance` target
conventions) log line when the passband-sanity check rejects a frequency. Today there is
zero trace on any relevance-gate rejection; this specific bug required manually
cross-referencing raw decode lines because of that silence. This is a cheap, targeted
fix to the observability gap this investigation hit, not a general logging audit.

## What stays unchanged

- Hound/Fox `partner_freq` split-frequency mode — untouched, separate path.
- Pre-establishment ambiguous-sender gating (`CallingCq`'s two arms, the fallback arm) —
  tight 15 Hz tolerance unchanged.
- Remote-TX security arms and `qso.security` warn-logging on callsign/direction mismatch
  — unchanged; this design only touches the *frequency* component of relevance, never
  the identity checks.
- No config surface, no new operator-facing flag — this is purely an internal
  correctness fix to existing automatic behavior.

## Testing

- **Regression test reproducing tonight exactly**: `RespondingToCq` latched at 1500 Hz;
  feed a `SignalReport` decoded at 937.5 Hz from the correct partner (`partner_freq =
  None`). Must transition to `SendingReport` (not silently drop), and the new state's
  `frequency` must equal 937.5, not 1500.
- At least one equivalent test for another of the seven relatch-eligible arms (e.g.
  `SendingReport`+`ReportAck`) — DX replies from a frequency far from the latch,
  transition must still fire and relatch.
- **Hound regression, unchanged**: `is_message_relevant_hound_keys_on_partner_freq` must
  stay green with no code changes to the test itself — it exercises the exact same
  `RespondingToCq`+`SignalReport` arm this fix touches, so it's the real boundary
  contract proving the Hound carve-out works. `hound_qsy_on_fox_report_full_exchange`
  (the full end-to-end Hound test) must also stay green unchanged.
- **`is_message_relevant_partner_freq_none_falls_back_to_state_freq` must be retargeted**,
  not left as-is: it currently uses `RespondingToCq`+`SignalReport` to assert a
  far-frequency frame is rejected — exactly the behavior this fix intentionally reverses
  for that arm. Change its message to `MessageType::ReportAck` (same `RespondingToCq`
  state, but that combo has no explicit identity-verified arm and still falls through to
  the unchanged fallback), preserving the test's real intent (distance-gate still applies
  where identity isn't independently re-verified) without asserting the behavior this fix
  removes.
- A passband-sanity regression test: a decode at an out-of-range frequency (e.g. -50 Hz
  or 3200 Hz) from the correct partner must still be rejected.
- The six existing direct unit-test callers of `determine_state_transition` need the two
  new arguments added (see Design #2) — confirm all six still pass unchanged.

## Non-goals

- Not touching `DecodeEffort`/decoder-backend behavior (unrelated, raised separately).
- Not building any new operator-facing control for this — it's default, automatic
  behavior, same as the rest of the QSO auto-sequencer.
- Not revisiting the ESTABLISHED/FREQ_TOLERANCE constants for the still-ambiguous arms —
  those are out of scope and already correctly tuned per the prior security review.
