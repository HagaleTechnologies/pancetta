# Batch 94 — TUI QSO-detail panel wired live

Date: 2026-06-12
Branch: `iter/2026-06-12-batch-94`
Follow-on from: Batch 93 TUI/Phase-5 gap assessment
(`research/notes/2026-06-12-batch93-tui-phase5-gaps.md`)

## Problem

The QSO-detail panel (`pancetta-tui/src/ui/qso_status.rs` — TX/RX/SNR/
progress sub-panels) was the last substantial dead panel. Its only data
source was `TuiMessage::QsoStateUpdate`, which the coordinator relay
never sent, so `app.qso_statuses` stayed empty forever and the panel
rendered STANDBY for the life of the process.

## Option chosen: B (enrich ActiveQsosSnapshot)

**Option A was confirmed dead code.** Grep over the workspace found zero
constructors of the bus-side `QsoMessage::QsoStateUpdate` — only the
declaration in `message_bus.rs`. Same for the TUI-side
`TuiMessage::QsoStateUpdate` (declared, consumed at `tui_runner.rs`,
never produced) and two TUI-local helpers that were never called
(`App::update_qso_state`, `ui::qso_status::update_qso_from_message` and
its callsign-extraction helpers). All four were removed.

**Option B implemented**: the QSO coordinator already pushes
`MessageType::ActiveQsosSnapshot` on every `QsoEvent::StateChanged` /
`QsoCompleted` / `QsoFailed`, and `QsoProgress` already carries
everything the panel needs (`messages: Vec<QsoMessage>` with
direction/raw_text/timestamp/signal_strength, `metadata.reports`
sent/received). So the snapshot item was enriched — a pure read of
existing engine state, zero behavioral change to the QSO engine, no new
bus message, no new TuiMessage variant.

## Data flow (after)

```
QsoManager (QsoEvent) ──► coordinator/qso.rs snapshot task
  └─ snapshot_item_from_progress(&QsoProgress)        [new pure fn]
       fields: callsign, state phase, freq, tx_parity,
               last_tx_text/at, last_rx_text/at (from progress.messages),
               snr_rx (last RX signal_strength),
               report_sent/received (metadata.reports),
               exchange_count (messages.len())
──► MessageType::ActiveQsosSnapshot (bus)
──► tui_relay map_qso_snapshot_item()                  [new pure fn]
──► TuiMessage::ActiveQsosUpdate (existing variant, enriched payload)
──► App::apply_active_qsos()                           [new]
       rebuilds BOTH app.active_qsos (banner, unchanged behavior)
       AND app.qso_statuses (detail panel, previously always empty)
──► ui/qso_status.rs renders state/last-messages/reports/SNR gauges
```

Stale-QSO removal follows the banner semantics exactly: each snapshot
replaces the whole list, completed/failed QSOs are filtered by
`get_active_qsos()` upstream, and an empty snapshot clears the panel
back to STANDBY.

## Panel now shows (per active QSO)

- callsign, state-machine phase ("wait rpt", "sending RR73", ...)
- audio frequency (Hz)
- last message exchanged each direction with time-ago
  (`TX: JA1ABC K5ARH EM10 (12s ago)` / `RX: K5ARH JA1ABC -12 (3s ago)`)
- reports sent/received (`Sent: -8  Rcvd: -12`)
- SNR gauges: TX = their report of us (report_received),
  RX = measured SNR of their last message (fallback: report_sent)
- multi-QSO table swaps the constant "FT8" Mode column for State

## Tests added (12 new, all seams)

- `coordinator/qso.rs::snapshot_tests` (4): detail-field derivation,
  latest-message-per-direction, no-callsign → None, empty history
- `coordinator/tui_relay.rs` (2): field-for-field snapshot→banner
  mapping, empty-detail mapping
- `pancetta-tui/app.rs` (3): apply_active_qsos populates panel,
  empty snapshot clears to STANDBY, snr_rx fallback to report_sent
- `pancetta-tui/tui_runner.rs` (1): ActiveQsosUpdate handler drives
  detail panel + clears on empty snapshot
- `pancetta-tui/ui/qso_status.rs` (1): format_direction_line
- (1 dead test removed with the dead extract_other_callsign helper)

## Verification

- `cargo build -p pancetta -p pancetta-tui` — "Compiling" confirmed
  after touch
- `cargo test --workspace --features transmit` — EXIT 0, all green
- `cargo clippy -p pancetta -p pancetta-tui` — no new warnings in
  touched files (pre-existing workspace warnings unchanged)
