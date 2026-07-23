# Mid-TX Abort/Restart Design

**Status:** Draft, pending review
**Date:** 2026-07-22
**Related:** [[project_symptom_c_multi_tx_deferred]] (predecessor, shipped PR #194),
docs/DECISIONS/tx-scheduling.md, `docs/security-deep-analysis-2026-06-17.md` (autonomous-TX
integrity findings)

## Problem

Once a transmission is keyed (PTT asserted / audio playing), pancetta commits to it: the freshest
content available at *key-time* is what goes out, full stop. The only existing escape hatch is the
operator's manual F8 abort, which simply kills the TX and leaves the operator (or the QSO engine)
to start over from scratch — no automatic re-key.

Two real gaps fall out of this:

1. **Manual override friction.** If the operator wants to respond to a different station (or send
   different content) while pancetta is already mid-TX on something else, today they must hit F8
   first and then separately issue the new send — two deliberate steps where WSJT-X operators are
   used to one (double-click a new station and it just happens).
2. **No autonomous recovery for exceptional rarity.** If an ATNO or new-per-band DXCC station is
   heard while pancetta is mid-TX on a routine QSO, there is no path to react — the rare station is
   either missed entirely or has to wait for the current QSO's TX to finish, by which point it may
   have moved on.

This is explicitly the queued follow-on to Symptom C (`[[project_symptom_c_multi_tx_deferred]]`):
Symptom C widened the *pre-PTT* decision window (zero on-air cost); this work is different in kind
— it can truncate an already-in-progress, already-over-the-air transmission, which carries real
FCC Part 97 / on-air-behavior consequences and is scoped and gated accordingly.

## Scope

**In scope:**
- Phase 1: manual trigger. Any operator `SendMessage`/`StartCq` command that arrives while a TX is
  in flight (regardless of whose TX it is — autonomous or manual, same QSO or different) and
  specifies different content than what's currently keyed automatically aborts and re-keys with the
  new content. No new keybinding; reuses existing commands.
- Phase 2 (separate PR, config-gated off by default until Phase 1 is validated on-air): autonomous
  trigger. A newly-decoded candidate scoring `PriorityTier::Atno` or `PriorityTier::PerBandDxccNew`,
  strictly higher-tier than whatever QSO is currently mid-TX, may trigger the same abort+re-key path.
- Mode-scaling `tx_late_max_ms` per protocol (FT8/FT4/FT2), since this feature makes the late-skip
  viability cliff matter routinely rather than only on rare late-arriving requests.
- A "resume after preemption" marker so a QSO bumped by Phase 2's autonomous preemption gets
  priority back once the preempting QSO completes, rather than re-entering open competition.
- Updating the CLAUDE.md invariant about frames reflecting the freshest message "at key-time."

**Explicitly out of scope (tracked separately):**
- The DX watchlist / heard-queue for sub-Atno-tier candidates — GitHub issue #197, its own future
  brainstorm.
- Any candidate below `PriorityTier::PerBandDxccNew` ever triggering an autonomous abort. Routine
  "somewhat higher priority" candidates are never worth truncating a live transmission for
  autonomously; they belong in the watchlist instead.
- A new manual keybinding — Phase 1 deliberately reuses existing send/CQ commands.

## Design

### Mechanism (shared by both phases)

The existing pre-PTT same-QSO "TX pivot" (`tx_pivot_target` / Step 4c in `coordinator/tx.rs`) is
the only existing precedent for swapping in-flight TX content, but it only runs once, before PTT is
asserted, and only ever targets the same `qso_id`. This design generalizes it rather than building a
parallel mechanism, to keep everything flowing through the one place that already finalizes TX
content (respecting this repo's single-scorer invariant):

1. **Trigger detection.** The TX worker already knows, in its own loop state, whether a TX is
   currently in flight (`InFlightTx`-equivalent: qso_id, message text, target slot). A new
   `TransmitRequest`/`MultiTransmitRequest` arriving on the worker's channel while a TX is in flight
   is the trigger event. Phase 1: any such arrival with content different from what's keyed
   qualifies unconditionally. Phase 2: only qualifies if the new request's `PriorityTier` is
   strictly higher than the in-flight QSO's tier, and is `Atno` or `PerBandDxccNew`.
2. **Interrupting cleanly.** On a qualifying trigger, the worker sets `abort_current_tx` itself (no
   operator keypress required) and stashes the new request instead of discarding it. This reuses the
   exact `interruptible_sleep` call sites that already exist at Steps 6 (pre-slot wait), 8 (audio
   playback wait), and 9 (PTT-off tail) — no new interruption plumbing.
3. **Re-keying, gated by `tx_late_max_ms`.** On the abort path, instead of just falling through to
   "wait for the next message," check for a stashed re-key request. Re-run `schedule_tx` against
   *now* (not the original slot-entry time) — the same function that already computes late-skip
   cursor math for any freshly-arriving request. If the current slot is still viable within the
   (mode-scaled) `tx_late_max_ms` budget, re-encode/modulate and re-assert PTT immediately with the
   skip-adjusted cursor. If not viable, leave PTT off and let the stashed request flow through the
   normal `schedule_tx` path for the next slot — no forcing.
4. **Bundle-add over full replace.** When `max_concurrent_qsos > 1`, before treating this as a
   same-slot replace, first attempt to fold the new request into a `MultiTransmitRequest` alongside
   the in-flight content (using its freshest text, via the existing pre-PTT pivot check) via the
   existing `encode_and_modulate_multi_tx` — reusing its existing pairwise frequency-separation
   checks. Fall back to a full single-item replace only if that fails (frequency collision, capacity,
   or `max_concurrent_qsos == 1`). This rule is identical for manual and autonomous triggers.

### `tx_late_max_ms` mode-scaling

Today `tx_late_max_ms` (default 8000ms) is one flat value fed identically into `schedule_tx` for
every protocol. Against FT4's 7.5s slot it is longer than the whole slot, so the "too late, defer"
branch can never fire for FT4 today — a latent gap this feature would otherwise inherit and make
routine instead of rare.

Compute an effective value at the same call sites that already read `tx_late_max_ms`:

```
tx_late_max_ms_effective(protocol) = tx_late_max_ms * (slot_ns(protocol) / SLOT_NS)
```

This mirrors the precedent already established by `coalesce_collect_window_ms`'s FT4/FT2 scaling.
FT8 stays exactly 8000ms (byte-identical). FT4 → 4000ms. FT2 → proportionally smaller once FT2 is
real. No new per-protocol config keys — `tx_late_max_ms` remains the single FT8-anchored config
value.

**Known side effect, called out explicitly:** per the existing FT4 TX Timing Margin investigation,
FT4's decode→route→re-key pipeline already runs tight on margin. Tightening the late-viability
cliff from "effectively unbounded" to a real ~4000ms cap means FT4 will start hitting "too late,
defer to next slot" where today it always attempts (and often badly truncates) — likely a net
improvement, but a real behavior change worth validating against real FT4 logs, not assumed.

### Resume-after-preemption marker (Phase 2 only)

Manual overrides need no special handling: the interrupted QSO simply missed its turn and competes
normally next cycle via existing retry/timeout logic, since the operator made a deliberate choice.

Phase 2's autonomous preemption is different: the bumped QSO should not have to win open
competition again to get its turn back. Add `preempted_at: Option<DateTime<Utc>>` to the QSO
object, set when it's the one aborted for an Atno/PerBandDxccNew preemption. The priority allocator
checks this each cycle: if set, and the preempting QSO is no longer active (completed, timed out, or
itself preempted by something rarer), the bumped QSO is scheduled ahead of open-competition
candidates on its next opportunity, and the marker clears on use. The marker expires after 4 slots
(~1 minute at FT8 cadence) if unused, so a QSO that goes idle/times out on its own in the meantime
doesn't hold a stale priority claim.

### Config

- New: `autonomous.mid_tx_preemption_enabled` (bool, default `false`). Gates Phase 2 only — Phase 1
  (manual) has no toggle; it's always on, matching the always-on expectation of the existing F8
  abort.
- No new `tx_late_max_ms`-per-protocol config keys; the scaling is computed, not configured.

### CLAUDE.md invariant update

> Every transmitted frame (single or multi-TX bundle item) reflects the freshest `MessageToSend`
> the QSO engine emitted for that qso_id at key-time.

becomes:

> Every transmitted frame (single or multi-TX bundle item) reflects the freshest `MessageToSend`
> the QSO engine emitted for that qso_id at key-time, or at the moment of an operator-triggered or
> (Atno/PerBandDxccNew-gated) autonomous mid-TX abort+re-key, whichever is later.

## Error handling

If the re-key's encode/modulate step fails, there is no "keep the original" fallback available (the
original audio was already torn down by the abort, unlike the pre-PTT pivot's failure path). Log a
warning, leave PTT off, and let the stashed request flow through the normal `schedule_tx` path for
the next slot — no additional dead air beyond what the abort itself already caused.

**Known side effect — cross-QSO `Replace` identity (accepted for Phase 1):** on the single-TX arm's
in-place `Replace` re-key, `supersede_and_rekey_or_bundle` swaps only `message_text` /
`frequency_offset` / `schedule` to the superseding request; the working `qso_id` intentionally
continues to track the ORIGINAL in-flight QSO's identity (it drives liveness / pivot-tombstone
bookkeeping). When the supersede is cross-QSO — the new content belongs to a different QSO than the
one being aborted — the re-keyed frame therefore transmits the NEW `message_text` while still
labelled with the OLD `qso_id`. Two consequences are accepted as Phase-1 characteristics: (a) a
display/telemetry mismatch (the TX strip / logs attribute the new text to the original QSO), and (b)
if the original QSO happens to reach a terminal state at that same instant, the drop-stale-TX gate
keyed on the (stale) `qso_id` could suppress the operator's deliberate manual override. Neither is
fixed here — Phase 1 keeps the original identity deliberately for bookkeeping simplicity. (Note: the
superseding request's `origin`, unlike its `qso_id`, is NOT carried over — it is threaded through so
the re-keyed frame is arm-gated and emitted under the new request's own origin; see the C1 note in
`coordinator/tx.rs`'s `SupersedeOutcome`.)

## Thrashing

Manual triggers are inherently operator-paced. Autonomous (Phase 2) triggers are self-limiting via
the "strictly higher tier than what's currently in flight" gate — an Atno-tier candidate cannot
retrigger against an already-in-flight Atno-tier preemption.

## Testing

- Unit tests on `tx_late_max_ms_effective`: FT8 byte-identical, FT4 exactly half, mirroring
  `coalesce_collect_window_scales_down_for_ft4`.
- Unit tests on trigger-gate logic in isolation: manual always qualifies; autonomous only strictly
  higher `PriorityTier` (Atno/PerBandDxccNew) qualifies.
- Extend the existing `interruptible_sleep`/pivot test suite to cover abort-during-playback (Step 8)
  re-key, not just the pre-PTT case.
- Loopback integration test: manual override mid-TX produces exactly one correct frame on the air,
  no double-send (reusing the existing double-PTT/pivot-duplicate tombstone machinery).
- Multi-TX bundle-add path: in-flight single TX + a qualifying second request with
  `max_concurrent_qsos > 1` becomes a bundle, not a replace, when frequency separation allows.
- Resume-marker expiry: bumped QSO gets priority back after the preempting QSO completes; marker
  clears on use and after the 4-slot timeout if unused.
- **On-air validation required before Phase 2 ships enabled**: same class of gate as the existing
  Phase-5 on-air acceptance criteria — add to meatspace-pending once Phase 1 is implemented.

## Delivery plan

1. PR 1 (Phase 1): manual trigger, shared mechanism (abort/re-key/bundle-add), `tx_late_max_ms`
   mode-scaling, CLAUDE.md invariant update. Ships with Phase 2 fully absent (no dead config either)
   — the shared mechanism is written generically enough that Phase 2 is additive, not a rewrite.
2. On-air validation of Phase 1 (meatspace-pending item).
3. PR 2 (Phase 2): autonomous Atno/PerBandDxccNew trigger, resume-after marker, config flag
   (default off). Enabled only after operator review of PR 1's on-air behavior.
