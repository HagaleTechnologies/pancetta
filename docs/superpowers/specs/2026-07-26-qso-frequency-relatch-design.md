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

### 1. Relevance gate: identity-verified arms skip the distance check

For every match arm in `is_message_relevant()` that already checks
`Self::is_partner(from, target) && self.is_us(to)` inside the arm (i.e. every
state-specific arm once a QSO is established — `RespondingToCq`+`SignalReport`,
`RespondingToCq`+`ReportAck`, `WaitingForReport`+`ReportAck`, `SendingReport`+`ReportAck`,
the `FinalConfirmation`/`SeventyThree` close arms, etc.), drop the
distance-from-latched-frequency check entirely. Replace it with a passband sanity bound:
reject only if the decoded frequency falls outside 200–2900 Hz (matching the existing
convention used for `freq_min_hz`/`freq_max_hz` elsewhere in this crate —
`frequency.rs`, `autonomous.rs`). This is a decode-garbage guard (protects against a rare
low-SNR mis-decode), not an identity check.

Leave the **tight gate completely unchanged** for the genuinely ambiguous
pre-identity-establishment arms:
- `CallingCq` + `CqResponse` (we don't yet know who's answering our CQ)
- `CallingCq` + `SignalReport` (bare-report answer, same ambiguity)
- the catch-all `_ => message_type.is_addressed_to(...)` fallback

These are exactly the cases the 2026-04-29 security review (catalog C-1, 50→15 Hz
tightening) targeted, and remain unchanged.

Hound/Fox `partner_freq` mode is unaffected — it's a separate, already-correct path
(`metadata.partner_freq` continues to override `qso_freq` as today) and doesn't
interact with this change.

### 2. Transition function: relatch to the incoming message's real frequency

Thread the incoming message's decoded `frequency` (`QsoMessage.frequency`, already
captured in `process_message_for_qso`) through to `determine_state_transition()` as a
new parameter. For the arms that advance the QSO on a message just received from the
identified partner — `SignalReport`, `ReportAck`, `FinalConfirmation`/`SeventyThree`
receipt arms — use that incoming frequency when constructing the new state's
`frequency` field, instead of carrying forward the prior latched value.

This means the QSO's tracked frequency (and therefore where we transmit our next reply)
relatches automatically to wherever the DX actually is — mirroring exactly what the
manual `RespondToCaller` override already does by hand, just automatic. Subsequent
decodes from the same partner then naturally pass the identity-verified gate against the
corrected value too (though after change #1 above, that check no longer gates on
distance anyway — the relatch's main value is correcting our own future TX offset to
match the DX, which is the real behavioral parity with the manual path).

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
  feed a `SignalReport` decoded at 937.5 Hz from the correct partner. Must transition to
  `SendingReport` (not silently drop), and the new state's `frequency` must equal 937.5,
  not 1500.
- Equivalent tests for the other identity-verified arms touched (`ReportAck` in
  `RespondingToCq`/`WaitingForReport`/`SendingReport`, `FinalConfirmation`/`SeventyThree`
  receipt arms) — same shape: DX replies from a frequency far from the latch, transition
  must still fire and relatch.
- Existing `CallingCq`-ambiguity tests and the Hound `partner_freq` routing tests
  (`is_message_relevant_hound_keys_on_partner_freq`,
  `is_message_relevant_partner_freq_none_falls_back_to_state_freq`) must stay green,
  unchanged — these are the two-boundary regression contracts of this fix.
- A passband-sanity regression test: a decode at an out-of-range frequency (e.g. -50 Hz
  or 3200 Hz) from the correct partner must still be rejected.

## Non-goals

- Not touching `DecodeEffort`/decoder-backend behavior (unrelated, raised separately).
- Not building any new operator-facing control for this — it's default, automatic
  behavior, same as the rest of the QSO auto-sequencer.
- Not revisiting the ESTABLISHED/FREQ_TOLERANCE constants for the still-ambiguous arms —
  those are out of scope and already correctly tuned per the prior security review.
