# Runtime FT8/FT4/FT2 Mode Switching — Design Spec

**Date:** 2026-07-05
**Status:** Proposed (awaiting operator review)
**Author:** Claude Sonnet 5 (under K5ARH supervision)

## Goal

Let the operator change the station-wide operating mode (FT8/FT4/FT2) **while pancetta is running**,
instead of only at startup via `[rig].mode` in `pancetta.toml`. One sentence: *the same atomics and
hot-reload mechanisms that already make band-switching and tier-preset changes work at runtime get
extended to cover mode, so switching is a config-file-free operator action, gated so it can never
happen mid-QSO.*

## Background

FT4 mode landed 2026-06-29 (`docs/superpowers/specs/2026-06-29-ft4-mode-design.md`) as a **startup-only**
selection: `[rig].mode` is read once at `Coordinator` construction into `active_protocol`, a plain
field, and never changes again. Changing modes today means editing the config file and restarting.

Digging into the FT4 work, most of the infrastructure a runtime toggle needs already exists, just
wired for one-time use:

- `active_slot_ns` / `active_decode_phase_ns` are **already atomics** (`Arc<AtomicI64>`), read live by
  the TX scheduler and decode-parity stamping — nothing currently writes to them after startup.
- `Ft8Config` is **already** `Arc<RwLock<Ft8Config>>`, and the decode loop (`coordinator/ft8.rs`)
  **already hot-rebuilds** `Ft8Decoder` when it notices `max_decode_passes`/`osd_depth` changed (the
  hb-216 S2 tier-preset mechanism) — a `try_read` each window, diffed against cached locals.
- TX encode/modulate (`coordinator/tx.rs`) is **already dispatched per-request** by protocol
  (`match protocol => Ft8Encoder::new() / Ft4 / Ft2`), so a live TX request already resolves its
  encoder from whatever protocol is current at request time.
- The DSP thread (`coordinator/dsp.rs`) already has a **band-switch flush** mechanism
  (`band_flush_decision`) that clears `ft8_buffer` and resets waterfall bookkeeping when the dial
  frequency crosses a band boundary mid-stream — the same shape a mode-triggered buffer-size change
  needs, just keyed on mode instead of dial.

So this isn't new mechanism so much as making four already-built "read this every window" checks
respond to one more value, plus wiring an operator-facing trigger and a safety gate.

## Design

### Shared state

`Coordinator::active_protocol` (currently a plain `pancetta_ft8::Protocol` field, `coordinator/mod.rs`)
becomes `active_protocol_mode: Arc<AtomicU8>` (0=Ft8, 1=Ft4, 2=Ft2), sitting beside the existing
`active_slot_ns`/`active_decode_phase_ns` atomics it always should have accompanied. No new lock type,
no new component/actor — every consumer (DSP thread, decode thread, TX worker, QSO task) keeps polling
the same shared atomics/`RwLock<Ft8Config>` it already polls today; this work makes the values behind
that shared state actually change after startup.

### `Coordinator::set_operating_mode`

The single entry point for a mode change, `set_operating_mode(new_mode: OperatingMode) ->
Result<(), ModeSwitchError>`:

1. Take `active_tx_qsos.read().await` and check it's empty — else return
   `ModeSwitchError::QsosActive(count)`. **Hold this read guard for the entire function body** (see
   Race safety below), not just the initial check.
2. Acquire `ft8_config.write().await` and set `.protocol` to the new protocol. This is the one fallible
   step (poisoned lock) and happens **before** any atomic is touched — see Ordering below.
3. Compute the new `slot_ns` / `decode_phase_ns` via the existing
   `derive_dsp_timing(&ProtocolParams::from_protocol(new_mode))` and store them into
   `active_slot_ns` / `active_decode_phase_ns` (infallible atomic stores).
4. Store `active_protocol_mode`.
5. Send `QsoMessage::SetOperatingMode(mode_str)` on the bus so the QSO component's task updates
   `QsoManager::config.active_mode` for QSOs opened from now on. Best-effort: a failed send is logged
   at `warn!` and does not fail the overall switch (the QSO task being down is already a bigger
   problem than a stale mode string).
6. Send `TuiMessage::ModeUpdate(mode_str)` so the TUI's title-bar chip updates immediately, and emit a
   `DiagnosticEvent`/status line confirming the switch.

**Ordering (fail-closed on poisoned lock).** Step 2 (the fallible one) happens before steps 3-4 (which
cannot fail). If the `ft8_config` lock is poisoned, the function returns
`ModeSwitchError::ConfigLockPoisoned` having touched nothing — DSP/decode/TX/QSO all keep seeing the
old mode consistently. This mirrors the remote-TX arm gate's fail-closed posture (by contrast with the
drop-stale-TX gate, which deliberately fails open since it isn't safety-critical) — a mode switch is
closer to the former: better to refuse than leave DSP and decode disagreeing about frame geometry.

**Race safety.** Holding the `active_tx_qsos` read guard across the whole function means any concurrent
registration of an already-created QSO (which needs a write lock on that same set to insert) blocks
until the switch completes or never starts if the initial check saw a non-empty set. This closes the
check-then-act race against a concurrent *write* to that set — the same class of race the auto-repark
feature's "zero `.await` between the live-QSO check and the write" invariant exists to prevent, without
needing a new lock. It is **not** airtight end-to-end, though: registration happens asynchronously,
slightly after a QSO is actually created (e.g. autonomous `respond_to_cq`, always-answer-callers), so a
QSO that is still being *created* concurrently with a switch — and hasn't reached its `active_tx_qsos`
insert yet — can slip through and end up running under the new mode's timing. This is a narrow,
low-severity residual window (the same class of bounded gap the codebase already accepts elsewhere,
e.g. the auto-repark feature's own documented residuals), not a full closure of the race.

### DSP thread — buffer resize on mode change

`dsp.rs` currently computes `dsp_window_samples`/`dsp_overlap_samples` **once** at thread spawn via
`derive_dsp_timing`, then holds them as fixed locals. Add:

- A pure `mode_flush_decision(cached_mode: u8, cur_mode: u8) -> bool`, structurally identical to
  `band_flush_decision` — same test shape, just diffing the protocol atomic instead of the dial atomic.
- Each loop iteration, load `active_protocol_mode` alongside the existing `cur_dial_hz` read (negligible
  added cost — already on this hot path for another atomic).
- On a mode flip: recompute `dsp_window_samples`/`dsp_overlap_samples`/decode-phase via
  `derive_dsp_timing`, clear `ft8_buffer` (old-mode audio isn't valid new-mode frame geometry, exactly
  like old-band audio isn't valid new-band content), and reset `last_live_wf_samples` — the same
  recovery the band-switch flush already performs. The next full window (~13s for FT8 / ~6.5s for FT4)
  rebuilds clean.

### Decode thread — extend the existing hot-rebuild check

`ft8.rs`'s tier-preset hot-rebuild (lines ~389-411) already diffs `cur_max`/`cur_osd` against cached
locals and rebuilds `Ft8Decoder` on mismatch. Add `cur_protocol` to that same diff and rebuild path —
one more field compared, the existing `Ft8Decoder::new(new_cfg)` branch already handles it.

### TX path

No behavior change needed — already protocol-dispatched per request. The only implementation-level
change is reading the renamed `active_protocol_mode` atomic at `tx.rs:804` instead of the old plain
`active_protocol` field.

### QSO manager mode stamping

`QsoManagerConfig.active_mode` (a plain `String`, currently set once at construction) gains a setter:
`QsoManager::set_active_mode(&mut self, mode: String)`. Routed via a new `QsoMessage::SetOperatingMode
(String)`, handled in the QSO component's task (the same task that already owns `&mut QsoManager` and
processes other `QsoMessage` variants). Only affects QSOs opened after the switch — the active-QSO gate
guarantees nothing is in-flight at switch time, so no metadata migration is needed for anything already
logged.

### Manual band-change mode-awareness

Closes the pre-existing TODO at `app.rs:1833-1840`. Today `apply_band_selection` always uses
`band.ft8_frequency` from the TUI's own band table, regardless of active mode (the autonomous
`ChangeBand` handler is already mode-aware via `Band::dial_for`/`ft4_frequency` and is untouched by this
work).

- The TUI already needs the live mode value for the title-bar chip (below), so `apply_band_selection`
  reads the same `station_info.mode` field.
- FT8 → `band.ft8_frequency` (unchanged). FT4 → `pancetta_core::Band::dial_for(true)`, the same call the
  autonomous handler uses; on a band with no standard FT4 sub-band (`dial_for` returns `None` — an
  existing, documented gap in the frequency table itself, e.g. 60m/160m/2m), fall back to the FT8
  frequency with a status-line note.
- FT2 → no standard dial-frequency table exists yet (FT2 remains blocked on the operator resolving two
  incompatible spec candidates — see Scope below). Manual band-change in FT2 mode falls back to the FT8
  dial with a one-time status-line note ("FT2 dial frequencies not yet defined, using FT8").

### Live mode + band display

`App::station_info.mode` (set once in `App::new()` from config) and `tui_relay.rs`'s
`relay_active_mode` (read once when the relay task starts, then stamped onto every decode view
forwarded to the TUI) are both currently startup-only snapshots — nothing updates them today because
nothing changes mode today. This work adds:

- A new `TuiMessage::ModeUpdate(String)`, sent from `set_operating_mode` (step 6 above), mirroring how
  `MessageType::TxPolicyStatus` → `TuiMessage::TxPolicyUpdate` already updates the TX-policy banner. The
  TUI updates `station_info.mode` on receipt.
- `tui_relay.rs`'s per-decode mode stamping switches from the one-time snapshot to reading the live
  `active_protocol_mode` atomic each time (same pattern as other per-decode atomic reads on that path).
- Display: the mode chip stops being FT8-hidden and always renders — bold, next to a similarly
  bold/embiggened band indicator in the title bar, both always visible rather than only on deviation
  from default (operator direction: "make it very obvious which mode we're in").

### Trigger

A new `TuiCommand::CycleOperatingMode`, bound to **Shift+M**, cycling FT8 → FT4 → FT2 → FT8 — the same
shape as the existing tri-state `g` (TX-policy) cycle. `tui_relay.rs` calls `set_operating_mode(next)`;
on `Err(QsosActive(n))` it pushes a transient operator status line ("can't switch mode: N QSO(s)
active"), the same mechanism already used for other refused actions (e.g. RespondOnly-blocked
initiations). No modal, no confirmation step.

### Autonomous interaction

No special-casing. The gate is "any QSO active right now," independent of whether autonomous mode is
enabled. If autonomous has an open QSO, the switch is refused exactly like a manual refusal; if it
doesn't, the switch proceeds and the next autonomous decision (which already reads `active_mode`/
`ft8_config` fresh each cycle) simply uses the new mode.

## Scope / non-goals (v1)

- **FT8 ⇄ FT4 switching is the validated path.** FT4 mode itself already shipped and is exercised
  end-to-end (2026-06-29). This spec's job is the *switching mechanism*, not re-proving FT4 correctness.
- **FT2 gets the switching infrastructure but not correctness validation.** FT2's protocol parameters
  are speculative (`protocol.rs` "just guesses" per existing backlog) pending the operator resolving two
  incompatible FT2 spec candidates. This work makes FT2 reachable via the cycle key and gives it the
  same buffer-flush/decoder-rebuild treatment as FT4, but does **not** validate FT2 decode/TX
  correctness — that's tracked separately (existing Tier-4 backlog item) and is explicitly deferred.
- **No persistence across restart.** `[rig].mode` in `pancetta.toml` is always the startup mode; a
  runtime switch is a session-only override, matching how TX-offset-hold and split are not persisted
  today either.
- **No queueing.** A switch requested while a QSO is active is refused outright, not deferred/auto-
  applied later. The operator retries once clear.
- **Not in scope, tracked separately:** an easier-than-manual-tune way to pick standard-but-less-common
  FT8/FT4 frequencies (distinct from default band cycling) — a related idea raised during this
  brainstorming session but explicitly deferred to its own future spec (see project memory
  `project_nondefault_freq_picker`).

## Risks / careful points

1. **Ordering must hold**: `ft8_config` write (fallible) before atomic stores (infallible), so a failure
   never leaves DSP/decode disagreeing with TX/QSO about the active mode. Test: force a poisoned lock in
   a unit test and assert zero atomic mutation.
2. **Race window**: the `active_tx_qsos` read guard must be held for the *entire* switch, not just the
   initial check — verified the same way the auto-repark safety contract is verified (inspection for
   zero `.await` between check and final write, plus a targeted concurrency test if feasible).
3. **DSP buffer flush loses up to one window of audio** on every mode switch — same cost as a band
   switch today, already accepted behavior, not a regression.
4. **FT2 dial-frequency gap**: falling back to the FT8 dial for FT2 manual band-changes is a stopgap,
   not a fix — flagged in-code and in the status line so it isn't mistaken for a real FT2 frequency
   table.
5. **Regression invariant**: no switch ever requested ⇒ byte-identical to today (mode=FT8 throughout,
   same as the existing FT4-mode regression invariant). Cheap to assert, worth it given how much shared
   state this touches.

## Testing

- **Unit (pure functions):** `mode_flush_decision` (mirrors `band_flush_decision`'s test suite); the
  mode-aware manual-band-dial resolver (FT4-with-table, FT4-without-table fallback, FT2 fallback).
- **Decode hot-rebuild:** extend the existing tier-preset-rebuild test to also flip `protocol` and
  assert `Ft8Decoder` rebuilds.
- **QsoManager:** `set_active_mode` unit test — QSOs opened after the call stamp the new mode (mirrors
  the existing `active_mode_ft4_stamps_metadata` test).
- **`coord_sim` (rig-level, real components):**
  1. Switch refused with a QSO active — assert error returned, atomics/config unchanged, in-flight QSO
     unaffected.
  2. Switch succeeds while idle — the next QSO opened afterward reflects the new slot timing/mode in its
     metadata.
  3. DSP buffer flush fires on a mode change (mirrors existing band-flush coverage).
- **TUI:** `CycleOperatingMode` keybinding test (mirrors `key_equals_emits_band_up`); `apply_band_
  selection`'s new mode-aware branch (FT4-known-band, FT4-unknown-band fallback, FT2 fallback).
- **Regression invariant:** mode=FT8, no switch requested ⇒ byte-identical to today (same style as the
  Hound `partner_freq: None` regression guard).

## Open questions for review

None outstanding — scope, gating semantics, trigger UX, persistence, and the manual-band-change bundling
were all settled during brainstorming (2026-07-05).
