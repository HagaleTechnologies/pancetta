# QSO Frequency Relatch v2 — Two-Strike Confirm Design

**Status:** Approved for planning
**Date:** 2026-07-27
**Supersedes:** `2026-07-26-qso-frequency-relatch-design.md` (broke an adversarial
anti-spoofing test — see that file's superseded notice)

## Problem (unchanged from v1 — still accurate)

Live 2026-07-26 (QSO with LU7LRP, `64043db4-5d33-4c9d-87df-b94dd50ad67b`): we called LU7LRP
at 1500 Hz. LU7LRP's own transmissions consistently landed at 937.5 Hz — a legitimate DX
operating on an independent, fixed TX offset, not RF drift. Their real signal-report reply
(`K5ARH LU7LRP -11`, decoded twice at SNR -19 and -17, ~30 seconds apart) was silently
discarded both times, and the QSO stayed stuck in `RespondingToCq`, re-transmitting the
original CQ-response call every cycle instead of advancing. It only recovered when the
operator manually triggered `RespondToCaller`, which relatches the QSO's tracked frequency
by hand. The same pattern shows up 3 times in the prior day's log, so this is recurring.

## Why v1's design was wrong

`is_message_relevant()`'s distance-from-latched-frequency check exists to defend against
exactly the scenario `pancetta-qso/tests/adversarial_3party.rs::b10_partner_call_used_by_other_station_discarded`
models: a station transmitting text that falsely claims a QSO partner's callsign
("K5ARH VB7F -10") from a different frequency. FT8 decoded text carries **no cryptographic
identity** — anyone can transmit a frame claiming any callsign — so frequency proximity to
where the QSO last genuinely heard its real partner is the *only* corroborating signal this
protocol has. v1 proposed treating a callsign+direction match as sufficient identity proof on
its own and skipping the distance check entirely for such matches; that directly reopens the
hole B10 exists to close. A single off-frequency identity-matching message cannot be safely
distinguished from a spoof — it can only be trusted once it repeats.

## Design

### Core mechanism: two-strike confirm before relatch

Add one field to `QsoMetadata` (`pancetta-qso/src/states.rs`):

```rust
/// The last off-latch frequency seen from this QSO's partner that didn't yet match
/// the relevance gate's tolerance. `None` normally. Set when an identity-matching
/// message arrives outside tolerance but inside the FT8 passband; cleared either when
/// a SECOND message confirms the same frequency (triggering a relatch) or when a
/// message arrives back within the existing tolerance (the drift resolved itself or
/// was a one-off). See `QsoManager::maybe_confirm_frequency_drift`.
#[serde(default)]
pub pending_freq_drift: Option<f64>,
```

Add a new method, `QsoManager::maybe_confirm_frequency_drift`, called from
`process_message_with_parity` **before** the existing `find_qsos_for_message` routing call
(which stays completely unmodified). For each active QSO:

1. Skip if `metadata.partner_freq.is_some()` (Hound/Fox — untouched, has its own dedicated
   QSY mechanism already).
2. Determine identity match using the SAME generic checks the codebase already has —
   not a per-arm enumeration:
   - `message_type.sender_callsign()` must equal (via `QsoManager::is_partner`, case-
     insensitive/compound-aware) `state.their_callsign()` — which is `None` for
     `CallingCq`/`Idle`, so those are naturally excluded (an established QSO is exactly
     `their_callsign().is_some()`, consistent with the existing "established" tolerance
     branch).
   - `message_type.is_addressed_to(&self.config.our_callsign)` must be true.
3. If no identity match: leave `pending_freq_drift` untouched, do nothing.
4. If identity match, get `qso_freq = state.frequency()` (the QSO's current latch) and
   compute `distance = (qso_freq - frequency).abs()`:
   - `distance <= ESTABLISHED_FREQ_TOLERANCE_HZ` (100 Hz, the existing constant): already
     within tolerance — clear `pending_freq_drift` to `None` (stale candidate no longer
     relevant) and do nothing else; the existing pipeline will route this normally.
   - `distance > ESTABLISHED_FREQ_TOLERANCE_HZ` and `frequency` inside the FT8 audio
     passband (200.0..=2900.0 Hz, matching the convention in `frequency.rs`/`autonomous.rs`):
     - If `metadata.pending_freq_drift` is `Some(f)` and `(f - frequency).abs() <= 15.0`
       (reusing the existing `FREQ_TOLERANCE_HZ` constant) — **confirmed**: this is the
       second consecutive sighting of the same new frequency. Relatch:
       - `metadata.frequency = frequency`
       - Reach into `progress.state` and overwrite whichever variant's own embedded
         `frequency` field is present — mirrors exactly the existing Hound-QSY block's
         technique (`pancetta-qso/src/qso_manager.rs`, search `hound_qsyed`, the
         `if let QsoState::SendingReport { frequency: ref mut state_freq, .. } = ...`
         pattern) — generalized to whichever state variant is actually active. Only
         `RespondingToCq`, `WaitingForReport`, `SendingReport`, `WaitingForConfirmation`,
         `SendingConfirmation` are reachable here (all carry a `frequency` field AND
         return `Some` from `their_callsign()`, which step 2's identity check already
         requires — `CallingCq`/`Idle` return `None` from `their_callsign()` and are
         structurally excluded before reaching this branch; `Completed`/`Failed` are
         excluded by the existing active-QSO filter this pre-pass reuses from
         `find_qsos_for_message`). This step is required because `is_message_relevant`'s
         distance check reads
         `state.frequency()` (the state enum's own field), not `metadata.frequency` —
         the two are separate storage that must be kept in sync manually, exactly as
         the Hound QSY code already has to do.
       - Clear `pending_freq_drift` to `None`.
       - `info!(target: "qso.freq_gate", ...)` — this is an operationally significant,
         visible event (a QSO's tracked frequency just moved), unlike v1's buried
         `debug!`.
     - Else (no pending candidate, or a different frequency than the pending one):
       set `pending_freq_drift = Some(frequency)` (start or reset the candidate). No
       state mutation. `debug!(target: "qso.freq_gate", ...)`.
   - `distance > ESTABLISHED_FREQ_TOLERANCE_HZ` and outside the passband: leave
     `pending_freq_drift` untouched (a passband-violating decode is likely garbage, not a
     real drift candidate — don't let noise reset a legitimate pending candidate).

After this pre-pass, `find_qsos_for_message`/`is_message_relevant`/
`determine_state_transition` run **completely unmodified**. When a relatch happened, the
just-confirmed message now matches the (already-updated) latch and routes through the
existing pipeline normally — the QSO advances exactly as it would if we'd called the DX at
the right frequency from the start.

### Security property preserved

B10's spoofed frame is a single occurrence: `metadata.pending_freq_drift` gets set once,
the message is still rejected by the (unmodified) existing gate exactly as before, and no
second confirming frame ever arrives in that test — `b10_partner_call_used_by_other_station_discarded`
passes with **zero changes to the test itself**, which is the real proof this design is sound.

### What stays unchanged

- `is_message_relevant()` — byte-for-byte unchanged from before v1 ever touched it.
- `determine_state_transition()` — byte-for-byte unchanged.
- Hound/Fox mode — untouched (`partner_freq.is_some()` guard skips the new mechanism
  entirely; Hound already has its own working QSY mechanism for exactly this problem).
- No new config surface, no operator-facing control.

### Real-incident validation

Traced against the actual 2026-07-26 log: first `SignalReport` from LU7LRP at 937.5 Hz
(19:22:13, latch was 1500) → `pending_freq_drift = Some(937.5)`, still dropped (identical to
today's behavior). Second `SignalReport` at 937.5 Hz (19:22:43, ~30s later) → matches the
pending candidate within 15 Hz → confirmed, relatch to 937.5, message routes normally,
QSO advances to `SendingReport`. This design would have recovered automatically about one
cycle after the real incident's actual manual-override point — a small, acceptable delay in
exchange for closing the spoofing hole completely.

## Testing

- **`b10_partner_call_used_by_other_station_discarded` must pass with zero changes to the
  test file** — the direct proof the security property holds.
- **Regression test reproducing the incident**: `RespondingToCq` latched at 1500 Hz; feed a
  `SignalReport` at 937.5 Hz (first sighting) — QSO must NOT advance, `pending_freq_drift`
  must be `Some(937.5)`. Feed a second `SignalReport` at 937.5 Hz — QSO must now advance to
  `SendingReport` with `frequency == 937.5` (checking both `metadata.frequency` and the
  state's own embedded field).
- **Single sighting never confirms**: one off-latch message, then a normal in-tolerance
  message arrives instead of a repeat — `pending_freq_drift` must clear, QSO must not have
  relatched, no spurious advance.
- **Different second frequency doesn't confirm**: first sighting at 937.5, "second" sighting
  at a different off-latch frequency (e.g. 1100) — must NOT confirm; the candidate resets to
  1100 instead, still rejected.
- **Passband-violating decode doesn't reset a legitimate pending candidate**: first sighting
  at 937.5 sets the candidate; a garbage decode at e.g. -50 Hz arrives next — candidate must
  remain `Some(937.5)` (not overwritten by the out-of-band noise); a genuine repeat at 937.5
  after that must still confirm.
- **Hound QSOs untouched**: `is_message_relevant_hound_keys_on_partner_freq` and
  `hound_qsy_on_fox_report_full_exchange` — zero changes, both must stay green (proves the
  `partner_freq.is_some()` guard works and the new mechanism never runs for Hound).
- Existing full `pancetta-qso` suite (403 tests as of the last known-good baseline) must stay
  green with zero other changes — this design touches no existing test besides adding new
  ones, which is itself a strong signal the scope is correctly isolated.

## Non-goals

- No configurable strike count or tolerance — 2 strikes / 15 Hz confirm bound are fixed,
  matching existing constants in this file. Revisit only if real-world use shows it's wrong.
- No expiry/timeout on a pending candidate — the QSO's own natural lifecycle (timeouts,
  supersession) already bounds how long stale state can matter; adding a separate timer is
  unnecessary complexity (YAGNI).
- Not touching `is_message_relevant`, `determine_state_transition`, or any of their existing
  tests.
