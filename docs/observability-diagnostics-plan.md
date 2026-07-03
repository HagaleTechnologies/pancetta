# Observability / diagnostics — implementation plan

Detailed plan to let the operator answer, **without hand-reading the log**: "why did
that QSO fail?", "why are we retrying / not transmitting?", and "is the station healthy
right now?" **Plan only — no code here.** Grounded in the current code; every hook cited
is real.

## Problem

The station logs richly but surfaces almost nothing. We recently diagnosed two on-air
QSO bugs only by hand-reading a 27,000-line log. The operator can *feel* problems (a QSO
stalls, we retry, TX is late) but cannot *see why*.

## Current state (what exists)

- **Logging:** `tracing` → daily-rotating file `~/.pancetta/logs/pancetta.log` (14-file
  retention); console layer only in `--headless` (in TUI mode the file is the *only*
  log). Plain text, **not JSON / not queryable**. There **is** a consistent `target:`
  vocabulary already in use — `qso.security` (22 sites), `tx.policy` (22), `qso`,
  `operator.override`, `agent.tx`, `qso.autonomous`, `priority`, etc. — but it is
  **write-only to the file**; nothing indexes or surfaces it.
- **Operator surfacing bottleneck:** `App.status_message: String`
  (`pancetta-tui/src/app.rs:618`) — a **single line, overwritten on every update**.
  `MessageType::Error` is constructed in only **two** places (audio-init failure; relay
  of an inbound bus error). Health warnings ("⚠ RF present but no decodes", "⚠ INPUT
  SILENT") compete for the same one line and are gone the next frame. There is **no
  event history, no notification stack, no Logs/Diagnostics panel**.
- **QSO failure data EXISTS and is thrown away at one spot.** `QsoState::Failed{reason,
  last_state}` + a rich `QsoFailureReason` enum (`Timeout, SignalLost, Duplicate,
  InvalidCallsign, FrequencyConflict, UserCancelled, Superseded, StationQrt,
  ProtocolError`) ride `QsoEvent::QsoFailed{reason, metadata}` (`qso_manager.rs:362`).
  The handler at `coordinator/qso.rs:1881` **ignores `reason`**, drops the QSO from the
  banner (it just vanishes), and `info!`s a generic line. The operator sees the QSO
  disappear with no reason.
- **Per-QSO timeline EXISTS in memory, never persisted.** `QsoProgress.state_history:
  Vec<StateTransition>` + `.messages` are populated on every transition
  (`qso_manager.rs:1965,3208`) but **dropped when the QSO leaves the active map**, and
  every DB/ADIF write hard-codes `state_history: vec![]`. No post-hoc reconstruction is
  possible.
- **Health:** `coordinator/health.rs` has component liveness (5s poll), pipeline atomics
  (`audio_alive`, `dsp_windows`, `total_decodes`, `last_decode_timestamp`), and good
  edge monitors (`RfNoDecodeMonitor` → "RF present but no decodes" / "input silent").
  All present, but **un-consolidated** and routed to the same overwrite-prone status
  line; `log_performance_stats` is log-only. No panic counters, no per-QSO success/fail
  tallies, no TX defer/drop counters.
- **TUI panels** already show live QSO ladder, now/next, watchdog countdown, call-count,
  TX-now chip, DX last-activity, SNR/timing, pipeline spans, autonomous state, rig chips
  — but only for *in-flight* QSOs and *current* state. Nothing historical or "why."

**Cross-cutting root cause:** there is no **retained, timestamped, filterable event
buffer**, and the one QSO-outcome hook discards its reason.

## Design

Three layers, each independently shippable, smallest first.

### Layer 1 — a retained diagnostic-event bus + panel (the foundation)

- **New bus variant** `MessageType::DiagnosticEvent { ts, target, level, text, qso_id:
  Option<..>, callsign: Option<String> }` (`pancetta/src/message_bus.rs:113`). `target`
  reuses the existing vocabulary (`qso.security`, `tx.policy`, `qso.autonomous`, …);
  `level` = Info/Warn/Error.
- **TUI side:** replace the single `status_message: String` with (a) the current status
  line *plus* (b) a bounded `VecDeque<DiagnosticEvent>` (cap ~500) in `App`
  (`app.rs:618`), fed via a new `TuiMessage::DiagnosticEvent` relayed in
  `tui_relay.rs:486-520`. Render a new **Diagnostics/Events panel** (a tab or a
  togglable pane in `pancetta-tui/src/ui/`) — timestamped, color-coded by level,
  filterable by `target` and by `callsign`/`qso_id`, scrollable. This is the durable
  home the overwrite-prone warnings finally get.
- **Emission:** the highest-value existing `warn!`/`info!` sites *also* emit a
  `DiagnosticEvent` (keep the `tracing` call for the file log; add the bus send).
  Priority sites: `qso.security` rejects, `tx.policy` stale-TX/suppression drops,
  autonomous skip/pounce decisions, keep-call watchdog retirement. A thin
  `emit_diagnostic!(bus, target, level, …)` helper avoids per-site boilerplate.

### Layer 2 — QSO outcomes surfaced + retained (directly answers "why did it fail?")

- At `coordinator/qso.rs:1881` (`QsoFailed`) **read `reason`** and emit a
  `DiagnosticEvent` (Warn) + a new **recent-QSO record** carrying `{callsign, outcome:
  Failed(reason)|Completed, last_state, freq, ts, brief timeline}`. Do the same for
  `QsoCompleted` (Info).
- **New retained "recent QSOs" surface** — a bounded ring (last N, e.g. 50) in `App`,
  rendered as a **Recent QSOs panel** (or folded into the Diagnostics panel filtered to
  QSO outcomes). Each row: callsign, outcome + reason, final rung reached, duration. An
  operator glances and sees "KJ5NJF — Failed: Timeout at SendingReport, 12 calls."
- **Persist the timeline (optional but high-value):** stop discarding
  `state_history`/`messages` — write them into a new `qso_events` table (or a
  JSON-per-QSO sidecar) so a completed/failed QSO's full "what we sent / what we heard /
  why we advanced" is reconstructable offline. Hooks: `states.rs:222`,
  `qso_manager.rs:3693` (stop the drop), `async_database.rs:868` (stop zeroing). Keep it
  behind a config flag if DB growth is a concern.

### Layer 3 — station-health-at-a-glance (answers "are we healthy?")

- A consolidated **Health panel / header** aggregating the already-computed signals:
  `PipelineHealth` (audio/dsp/decode liveness + staleness), `component_status` map,
  `RfNoDecodeMonitor` edges, plus **new lightweight counters** (per-session QSOs
  completed/failed-by-reason, TX attempts/defers/drops, decode-panic count which already
  exists as `DECODE_PANIC_COUNT`). Green/amber/red per subsystem with the last event
  time. This replaces "scattered across the status bar + chips" with one glanceable
  view.
- Feed it from the existing 2s health tick (`tui_relay.rs:537`), no new polling.

## Touch points (exact hooks)

| Change | File:line |
|---|---|
| New `DiagnosticEvent` + `RecentQsoOutcome` bus variants | `pancetta/src/message_bus.rs:113` |
| Surface QSO failure `reason` (today ignored) | `pancetta/src/coordinator/qso.rs:1881` |
| Relay mapping bus→TUI | `pancetta/src/coordinator/tui_relay.rs:486-520` |
| Event buffer + recent-QSO ring instead of single String | `pancetta-tui/src/app.rs:618` |
| New Diagnostics / Recent-QSO / Health panels | `pancetta-tui/src/ui/` (new render fns + layout region/tab) |
| Persist per-QSO timeline (Layer 2 opt) | `pancetta-qso/src/states.rs:222`, `qso_manager.rs:3693`, `async_database.rs:868` |
| Health aggregation | `pancetta/src/coordinator/health.rs:106-267` |
| Emission helper at existing log sites | `qso.security`(22)/`tx.policy`(22)/autonomous sites |

## Tests

- Unit: `DiagnosticEvent` buffer is bounded (ring eviction), filter-by-target/callsign.
- Unit: `QsoFailed` handler emits an outcome event carrying the correct `reason` +
  callsign (drives the coordinator handler with each `QsoFailureReason`).
- `coord_sim`: a QSO driven to `Failed{Timeout}` produces a retained recent-QSO record
  and a Warn diagnostic (not just a vanished banner).
- Snapshot/gold test of the Diagnostics panel render for a mixed event stream.
- Timeline persistence round-trip (Layer 2): write + reload a QSO's `state_history`.

## Risks / rollout

- **Bus volume:** don't emit a diagnostic per decode — gate to state changes,
  drops, rejects, and outcomes. Keep the `tracing` file log as the firehose; the bus
  event is the *curated* stream.
- **DB growth (Layer 2 persistence):** behind a config flag; bound the table / rotate.
- **Additive-only:** new `MessageType` variants + new panels; no change to existing
  decode/QSO/TX behavior. Ship Layer 1 first (foundation + the highest-value emission
  sites), then Layer 2 (QSO outcomes — the direct answer to the operator's question),
  then Layer 3 (health panel).
- Gate through the existing TUI test harness; no rig/on-air dependency.
