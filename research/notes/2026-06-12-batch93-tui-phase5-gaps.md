# Batch 93 — Phase 5 TUI gaps: autonomous toggle/panel + TX-active badge

Date: 2026-06-12
Branch: `iter/2026-06-12-batch-93`
Scope: operator-facing TUI wiring only. No TX/PTT behavior changed — both
new mechanisms are observers (badge) or gate-flippers (toggle); neither
can start a transmission directly.

## Gap 1 — autonomous toggle + live `[AUTO]` panel (safety-recovery)

### What was broken

- `tui_relay.rs` flattened `MessageType::AutonomousStatus` into a
  transient status-bar string; `app.autonomous_status` stayed `None`
  forever, so the `station_info.rs` `[AUTO]` panel always rendered the
  muted "Disabled" placeholder.
- The `a` key called `app.toggle_autonomous()` — TUI-local state only.
  No `TuiCommand` existed to reach the coordinator, so after a Shift+Q
  emergency stop (which clears `autonomous_enabled_runtime`) there was
  **no way to resume autonomous operation without restarting pancetta**,
  despite the banner text saying "Press `a` to re-enable autonomous."

### What was wired

1. **`TuiMessage::AutonomousStatusUpdate(AutonomousStatus)`** — the relay
   now forwards the structured `AutonomousStatusData` (fields already
   aligned 1:1 with the TUI struct). The status-bar text line is kept
   (additive). New relay helper `map_autonomous_status(data, gate)`
   AND-s the qso-engine's internal `enabled` with the runtime gate, so
   the panel shows what the station will actually do: after Shift+Q the
   engine still reports `enabled: true` (it keeps state for clean
   resume) but the panel correctly shows disabled.
2. **`TuiCommand::ToggleAutonomous`** — `a` key sends it (plus an
   optimistic local flip and clears the operator-stop banner). The
   command-forwarding loop flips the SAME `autonomous_enabled_runtime`
   `Arc<AtomicBool>` that `OperatorEmergencyStop` clears — symmetric by
   construction. Confirmation goes back immediately via a direct
   `StatusUpdate`; the authoritative panel update follows on the next
   autonomous slot tick (≤15 s; the qso engine emits one
   `StatusUpdate` action per slot). Config-disabled case is handled
   honestly: if `config.autonomous.enabled = false` there is no
   decision loop running at all, so the toggle reports "Autonomous
   disabled in config — restart with autonomous.enabled=true" and does
   not flip the gate.
3. The `[AUTO]` panel (`station_info.rs:render_autonomous_status`)
   needed no change — it renders from `app.autonomous_status`, which
   now gets populated.

### End-to-end Shift+Q → `a` recovery trace

1. **Shift+Q** (`tui_runner.rs` `KeyCode::Char('Q')`): sets
   `app.stopped_by_operator = true` (banner, no round-trip) and sends
   `TuiCommand::OperatorEmergencyStop`.
2. **Coordinator cmd loop** (`tui_relay.rs`): stores
   `abort_current_tx = true` (TX worker's `interruptible_sleep` wakes
   ≤50 ms → `PttGuard` drop → PTT off → new `TxStatusGuard` drop →
   badge clears), stores `autonomous_enabled_runtime = false`, stops
   repeating CQ, cancels tune. Logs WARN `target=operator.override`.
3. **Autonomous loop** (`coordinator/autonomous.rs`): every slot, gate
   checked before TX dispatch — decision-engine TX items dropped while
   closed (engine state preserved). Relay AND-s the gate into the next
   `AutonomousStatusUpdate` → panel shows disabled.
4. **`a`** (`tui_runner.rs` `KeyCode::Char('a')`): optimistic local
   flip, clears `stopped_by_operator` banner, sends
   `TuiCommand::ToggleAutonomous`.
5. **Coordinator cmd loop**: flips `autonomous_enabled_runtime`
   false→true (same atomic, opposite direction), WARN
   `target=operator.override` "Operator re-enabled autonomous TX",
   immediate `StatusUpdate` confirmation to the TUI.
6. **Next slot tick**: gate check passes; autonomous TX dispatch
   resumes through its own gates (slot parity, priority, QSO state).
   The toggle itself never queues a transmission. Next status emission
   renders the panel enabled again.

Gate semantics covered by `tui_relay_tests::emergency_stop_then_toggle_reopens_gate`
(the AtomicBool IS the seam — both handlers touch only it); mapping
covered by 3 `map_autonomous_status` tests; key/command/message seams by
4 new pancetta-tui tests.

## Gap 2 — TX-active indicator

`app.is_transmitting` was initialized false and never set; the
title-bar " TX " badge (`ui/mod.rs:139`) never lit.

Wired: new `MessageType::TxStatus { active: bool }` +
`TuiMessage::TxStatus`. The TX worker (`coordinator/tx.rs`) sends
`active: true` right after PTT assert in all three arms
(TransmitRequest, MultiTransmitRequest, TuneRequest) and constructs a
`TxStatusGuard` — an observer-only RAII guard whose `Drop` sends
`active: false`. Because abort paths (`F8`/Shift+Q `continue`, shutdown
`break`) and normal completion all exit the arm scope, the badge clears
on every path, mirroring exactly how `PttGuard` guarantees PTT-off.
Strictly observational: no PTT, audio, or scheduling touched. Headless:
send is best-effort (debug log on failure), same as the existing
AutonomousStatus emission.

Manual `p` (TogglePtt, direct rig PTT with no audio) intentionally does
NOT light the badge — it brackets pancetta-generated transmissions only.

## Gap 3 — CLAUDE.md

pancetta-tui row updated to: "Wired to pipeline (default UI;
`--headless` to disable); live autonomous panel + `a` toggle (Shift+Q
recovery) + TX-active badge; QSO-detail panel stubbed". QSO-detail
panel is Batch 94.

## Tests

- pancetta-tui `tui_runner::key_tests`: +4 (a-key emits toggle; a-key
  clears banner + emits toggle; AutonomousStatusUpdate populates app
  state; TxStatus sets/clears `is_transmitting`).
- pancetta `coordinator::tui_relay::tui_relay_tests`: +4 (mapping
  forwards all fields; closed gate renders disabled; engine-disabled
  wins; emergency-stop→toggle reopens gate).
- Full `cargo test --workspace --features transmit` green (exit 0).
