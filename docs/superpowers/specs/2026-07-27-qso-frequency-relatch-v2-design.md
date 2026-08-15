# QSO Frequency Relatch v2 — Two-Strike Confirm Design

**Status:** Approved for planning
**Date:** 2026-07-27
**Updated:** 2026-07-28 — final whole-branch review found the two-strike mechanism as
originally specced below could be satisfied by a SINGLE physical transmission (the
hb-091 scoped fast-path decodes one audio window twice and forwards both copies to the
QSO component before the standard pipeline's dedup point). See "Duplicate-delivery
hardening" below for the fix actually shipped; the original design below is otherwise
unchanged and still accurate.
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
/// The last off-latch frequency seen from this QSO's partner, together with the
/// timestamp it was FIRST noted, that didn't yet match the relevance gate's
/// tolerance. `None` normally. Set when an identity-matching message arrives outside
/// tolerance but inside the FT8 passband; cleared either when a SECOND message, at
/// least `DRIFT_CONFIRM_MIN_GAP` (5s) after the ORIGINAL timestamp, confirms the same
/// frequency (triggering a relatch) or when a message arrives back within the
/// existing tolerance. See `QsoManager::maybe_confirm_frequency_drift_at`. The
/// timestamp requirement exists because a decode-pipeline duplicate of the SAME
/// transmission (not a second, separate one) must never be able to satisfy
/// confirmation on its own — see "Duplicate-delivery hardening" below.
#[serde(default)]
pub pending_freq_drift: Option<(f64, DateTime<Utc>)>,
```

Add a new method, `QsoManager::maybe_confirm_frequency_drift`, called from
`process_message_with_parity` **before** the existing `find_qsos_for_message` routing call
(which stays completely unmodified). For each active QSO:

1. **[Updated by PAN-12, 2026-08-14 — see "Hound-only skip, not partner_freq" below]** Skip
   if `metadata.hound` is `true` (genuine Hound/Fox — untouched, has its own dedicated QSY
   mechanism already). `metadata.partner_freq.is_some()` alone is **not** the discriminator:
   ordinary manual QSOs also set `partner_freq` after an offset hold, a collision nudge, or a
   TX-ceiling clamp, and PAN-12 (PR #247, "relatch split-TX partner frequency") extended this
   mechanism to relatch those too — see step 4's split-TX branch below.
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
   - `distance > ESTABLISHED_FREQ_TOLERANCE_HZ` and `frequency` inside a dedicated
     **RX-plausibility** bound (`DRIFT_CANDIDATE_MIN_HZ..=DRIFT_CANDIDATE_MAX_HZ`,
     200.0..=2900.0 Hz — matching the convention in `frequency.rs`/`autonomous.rs`):
     this is deliberately a *decode-garbage sanity filter on where we might plausibly
     hear a real signal*, and is deliberately WIDER than this file's `TX_OFFSET_MIN_HZ`/
     `TX_OFFSET_MAX_HZ` (300–2700 Hz, our own preferred range for autonomously *picking*
     a fresh TX offset out of thin air). Do not conflate the two: responding to a CQ
     already ties our reply's TX offset to wherever we decoded that CQ today, unclamped,
     for any DX up to 2900-ish Hz (this app has real operational history of DX above
     2500 Hz — see the frequency-clamp bug fixed in PR #202) — the relatch mechanism
     must preserve that same behavior, not narrow it to `TX_OFFSET_*`.
     - If `metadata.pending_freq_drift` is `Some((f, t))`, `(f - frequency).abs() <= 15.0`
       (reusing the existing `FREQ_TOLERANCE_HZ` constant), **and** the confirming
       sighting's timestamp is at least `DRIFT_CONFIRM_MIN_GAP` (5 real seconds) after
       `t` — **confirmed**: this is a second, genuinely separate transmission at the
       same new frequency. Relatch, branching on whether this is a split-TX QSO
       (**[Updated by PAN-12]** — `metadata.partner_freq.is_some()` at the time of the
       relatch, independent of `metadata.hound`, since step 1's skip only excludes genuine
       Hound/Fox, not every `partner_freq.is_some()` QSO):
       - **Split-TX (`metadata.partner_freq.is_some()`, non-Hound)**: relatch **only**
         `metadata.partner_freq = Some(frequency)` — our own TX offset
         (`metadata.frequency` and the state's embedded `frequency` field) is
         **deliberately left untouched**, since split-TX means we intentionally TX and RX
         on different offsets (an operator's offset hold, a collision nudge, or a
         TX-ceiling clamp) and this mechanism only tracks where we *hear* the DX, never
         where we key. `old_state == new_state` is passed to `emit_state_change` (see
         below) since nothing in the state enum changed.
       - **Ordinary Tx=Rx (`metadata.partner_freq.is_none()`)**: relatch both
         `metadata.frequency = frequency` **and** reach into `progress.state` to overwrite
         whichever variant's own embedded `frequency` field is present — mirrors exactly
         the existing Hound-QSY block's technique (`pancetta-qso/src/qso_manager.rs`,
         search `hound_qsyed`, the
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
       - Both branches: clear `pending_freq_drift` to `None`;
         `info!(target: "qso.freq_gate", ...)` — this is an operationally significant,
         visible event (a QSO's tracked frequency just moved), unlike v1's buried
         `debug!`; emit `QsoEvent::StateChanged` (capture the pre-mutation state as
         `old_state`, call `self.emit_state_change(qso_id, old_state, new_state)` after
         mutating — for the split-TX branch `old_state`/`new_state` are identical). This
         is load-bearing, not cosmetic, in both branches: the coordinator's
         `StateChanged` handler refreshes `active_tx_offsets` and the hb-091 scoped
         fast-path's own decoder frequency hint (`qso_freq_state` /
         `active_qso_freq_hz`) by re-reading the QSO's current frequency
         (`metadata.partner_freq` for split-TX, `metadata.frequency` otherwise) —
         without this emission, a relatched QSO would leave the scoped decoder pointed
         at the stale pre-relatch offset even though the relevance gate has already
         moved.
     - Else (no pending candidate, or a different frequency than the pending one):
       set `pending_freq_drift = Some((frequency, now))` (start or reset the candidate).
       **If the pending candidate already has this SAME frequency** (within 15 Hz) but
       the gap requirement wasn't met yet — leave it untouched, do NOT overwrite the
       timestamp (see "Duplicate-delivery hardening" below for why). No state mutation.
       `debug!(target: "qso.freq_gate", ...)`.
   - `distance > ESTABLISHED_FREQ_TOLERANCE_HZ` and outside the RX-plausibility bound:
     leave `pending_freq_drift` untouched (a passband-violating decode is likely
     garbage, not a real drift candidate — don't let noise reset a legitimate pending
     candidate).

### Duplicate-delivery hardening (added 2026-07-28, post-review)

`pancetta/src/coordinator/ft8.rs`'s hb-091 scoped fast-path decodes the same audio
window twice — once via a scoped, narrow-frequency-range decode dispatched immediately
to the QSO component, and again via the standard full pipeline shortly after — as a
deliberate latency optimization. The code's own comment states dedup was assumed to
happen "at `is_message_relevant`" (rejecting a duplicate because the QSO's state has
already advanced past it). This pre-pass runs **before** that dedup point and has no
such protection: two decode-pipeline copies of ONE physical transmission, landing
milliseconds apart, would otherwise satisfy "two strikes" on their own — reopening
exactly the spoofing hole this whole design exists to close, reached via decode
duplication instead of the frequency gate directly.

Fix: `pending_freq_drift` carries a timestamp (see the field above), and confirmation
requires the second sighting to land **at least 5 real seconds** after the pending
candidate's ORIGINAL timestamp — comfortably below FT4's 7.5s slot period (the
shortest slot this app supports), so two genuinely separate transmissions always clear
it while two same-window decode copies (milliseconds apart) never do. A same-frequency
sighting arriving again within the gap does NOT reset the timestamp (only a
DIFFERENT-frequency sighting does) — otherwise repeated fast redeliveries could push
confirmation out indefinitely; a real DX's next genuine transmission will still land
≥5s after the original sighting and confirm normally, one slot later than it otherwise
would have.

Implementation mirrors this file's existing `check_timeouts`/`check_timeouts_at` split
for testability: `maybe_confirm_frequency_drift(&self, message_type, frequency)` is a
thin wrapper over `maybe_confirm_frequency_drift_at(&self, message_type, frequency,
now: DateTime<Utc>)`, which contains the real logic and is what tests call directly
with constructed timestamps (no wall-clock sleeps needed for the core logic; the two
existing end-to-end tests that need a real elapsed gap use `tokio::time::sleep`
deliberately, since the production code reads real `Utc::now()`, not a mockable clock).

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
- Genuine Hound/Fox mode — untouched (**[Updated by PAN-12]** the skip guard is
  `metadata.hound`, not `partner_freq.is_some()` — see step 1 above; Hound already has its
  own working QSY mechanism for exactly this problem). Ordinary non-Hound split-TX QSOs
  (offset holds, collision nudges, TX-ceiling clamps) are **not** excluded — PAN-12 extended
  this mechanism to relatch `metadata.partner_freq` for those too, leaving our own TX offset
  untouched (see step 4's split-TX branch above).
- No new config surface, no operator-facing control.

### Real-incident validation

Traced against the actual 2026-07-26 log: first `SignalReport` from LU7LRP at 937.5 Hz
(19:22:13, latch was 1500) → `pending_freq_drift = Some(937.5)`, still dropped (identical to
today's behavior). Second `SignalReport` at 937.5 Hz (19:22:43, ~30s later — comfortably past the 5s
minimum gap) → matches the pending candidate within 15 Hz → confirmed, relatch to
937.5, message routes normally,
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
- **Genuine Hound QSOs untouched**: `is_message_relevant_hound_keys_on_partner_freq` and
  `hound_qsy_on_fox_report_full_exchange` — zero changes, both must stay green (proves the
  `metadata.hound` guard works and the new mechanism never runs for genuine Hound/Fox).
  **[Added by PAN-12]** `genuine_hound_qso_is_still_skipped_by_drift_confirm` covers the same
  property directly against `maybe_confirm_frequency_drift_at`.
- **[Added by PAN-12] Non-Hound split-TX QSOs relatch `partner_freq` only**:
  `clamped_split_tx_qso_relatches_partner_freq_on_confirmed_drift` and
  `held_offset_qso_with_dx_on_partner_freq_never_drifts` — a split-TX QSO created by a
  TX-ceiling clamp or an offset hold confirms a drift in where we *hear* the DX
  (`metadata.partner_freq`) while `metadata.frequency` (our TX offset) and the state's own
  embedded `frequency` field are never touched.
- **Duplicate-delivery does not confirm**: two sightings at the same off-latch frequency,
  milliseconds apart (via `maybe_confirm_frequency_drift_at` with explicit timestamps) —
  must NOT relatch, and the pending candidate's timestamp must remain the ORIGINAL one, not
  the duplicate's. A third sighting ≥5s after the original must then confirm normally.
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
