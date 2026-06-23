# Unwrap Hardening + TUI Pipeline Completion

_Design spec — 2026-03-31_

---

## Overview

Two independent workstreams executed in parallel:

1. **Unwrap hardening**: Fix ~15-20 genuinely dangerous `unwrap()`/`expect()` calls in production code across `pancetta`, `pancetta-qso`, and `pancetta-dx`.
2. **TUI pipeline completion**: Fix data enrichment gaps and wire audio monitoring so the TUI is functional for receive-only operation end-to-end.

---

## Workstream 1: Unwrap Hardening

### Scope

Fix only the dangerous unwraps identified in the 2026-03-31 project audit. Leave safe unwraps (hardcoded regex compilation, known-good coordinates) alone.

### Changes

#### pancetta/src/main.rs

| Line | Current | Fix |
|------|---------|-----|
| 279 | `Signals::new(&[SIGINT]).expect("Failed to register signal handler")` | Replace with `?` — function returns `Result` |
| 295 | `signal::ctrl_c().await.expect("Failed to listen for ctrl+c")` | Replace with `?` |

#### pancetta-qso/src/statistics.rs

| Lines | Current | Fix |
|-------|---------|-----|
| 1023-1041 | `from_ymd_opt(year, 1, 1).unwrap().and_hms_opt(0,0,0).unwrap()` | Chain with `.ok_or_else(\|\| StatisticsError::InvalidDate(year))?` |
| 1170 | `date.and_hms_opt(0,0,0).unwrap()` | `.ok_or_else(\|\| StatisticsError::InvalidDate(...))?` |
| 1604, 1606-1607 | `current_session.first().unwrap()` / `.last().unwrap()` | Change to `.expect("checked non-empty above")` — the emptiness check is present but distant |
| 1622-1624 | Same pattern | Same fix |

#### pancetta-qso/src/autonomous.rs

| Line | Current | Fix |
|------|---------|-----|
| 185 | `self.our_slot.unwrap()` | Guard with `if let Some(slot) = self.our_slot` |
| 639 | `a.partial_cmp(b).unwrap()` | `.unwrap_or(std::cmp::Ordering::Equal)` to handle NaN |

#### pancetta-qso/src/exchange.rs

| Lines | Current | Fix |
|-------|---------|-----|
| 395, 441-442 | `captures.get(N).unwrap().as_str()` | `.ok_or(ExchangeError::MissingCapture { group: N })?` or use `if let` |
| 476 | `self.contest_mode.as_ref().unwrap()` | `.ok_or(ExchangeError::NoContestMode)?` |

#### pancetta-dx/src/geography.rs

| Lines | Current | Fix |
|-------|---------|-----|
| 467, 471 | `date.and_hms_opt(0,0,0).unwrap()` | `.ok_or(GeoError::InvalidTime)?` |
| 493, 512, 521, 540 | `and_hms_opt(...).unwrap()` inside `unwrap_or_else` | Refactor to use `.and_then()` chain or provide a safe constant fallback |

### Error Type Additions

Where a crate lacks an error variant for the new case, add it to the existing error enum:

- `StatisticsError::InvalidDate(i32)` — if not already present
- `ExchangeError::MissingCapture { group: usize }` — if not already present
- `ExchangeError::NoContestMode` — if not already present
- `GeoError::InvalidTime` — if not already present

Do not create new error enums. Extend existing ones.

### Testing

Run existing tests after each file. No new tests needed — these are error-path improvements that should not change happy-path behavior.

### Out of Scope

- Unwraps in test code (acceptable)
- Safe unwraps: `Regex::new("literal").unwrap()`, `Coordinate::new(40.71, -74.00).unwrap()`
- Creating new per-crate error types where none exist

---

## Workstream 2: TUI Pipeline Completion

### Goal

Make the TUI functional for receive-only operation when launched via the coordinator.

### Change 1: Fix Hardcoded Operating Frequency

**File**: `pancetta/src/coordinator.rs` (~line 886)

**Current**: `frequency: 14.074` hardcoded in the relay.

**Fix**:
- Add `operating_frequency_mhz: f64` field to coordinator state (or the relay task's captured state)
- Initialize from config (`station.frequency` or equivalent)
- Update when `MessageType::RigControl(RigControlMessage::FrequencyResponse { frequency, .. })` arrives on the message bus
- Pass current value into `DecodedMessageView.frequency` in the relay

### Change 2: Compute Distance and Bearing

**File**: `pancetta/src/coordinator.rs` (relay task, ~lines 883-897)

**Current**: `distance: None, bearing: None` in every `DecodedMessageView`.

**Fix**:
- Capture station grid square from config in the relay task
- When a decoded message has `grid_square: Some(grid)`, compute:
  - Great-circle distance using `pancetta-dx::geography` utilities
  - Bearing using the same utilities
- Populate `distance: Some(km)` and `bearing: Some(degrees)` in `DecodedMessageView`
- If station grid is not configured or remote grid is absent, leave as `None`

**Dependency**: `pancetta-dx` must be added as a dependency of the `pancetta` crate if not already present.

### Change 3: Wire Audio Monitoring

**File**: `pancetta-tui/src/app.rs` (~line 427)

**Current**: `start_audio_monitoring()` is a stub — sets `self.is_monitoring = true` but does nothing.

**Fix**:
- When TUI runs via coordinator (the primary path): the coordinator already creates audio→DSP→FT8→TUI channels and spawns the pipeline. The TUI consumes from `tui_msg_rx`. The pipeline setup is the coordinator's responsibility, not the TUI's.
- Replace the stub body with a log message confirming monitoring is active. The stub's `self.is_monitoring = true` flag is correct — the coordinator is what actually starts audio capture. The TUI just needs to know it should be displaying incoming data.
- Add a comment in the stub explaining that pipeline setup is handled by the coordinator.
- The standalone `pancetta-tui` binary path (running TUI without coordinator) is out of scope. Add a note to the Future Work table.

### Data Flow After Changes

```
Audio device
  → audio_to_dsp_tx [Vec<f32>]
  → DSP pipeline
  → dsp_to_ft8_tx [Vec<f32>]
  → FT8 Decoder
  → ft8_to_tui_tx [DecodedMessage]
  → Coordinator relay (enriches: real frequency, distance, bearing)
  → tui_msg_tx [TuiMessage::DecodedMessage(DecodedMessageView)]
  → TUI renders: band activity, waterfall, DX hunter
```

### Testing

- Run existing FT8 and coordinator tests
- Manual verification: run `pancetta run --wav <file>` and confirm TUI displays correct frequency, distance, bearing
- No new unit tests for relay logic (it's glue code) unless complexity warrants it

---

## Future Work (Out of Scope — Tracked as Pending)

These TUI features are not in this sprint but are captured for future implementation:

| Item | Description | Priority |
|------|-------------|----------|
| Mouse handling | Scroll band activity, click callsigns to select/call | Medium |
| Help panel (F1) | Keyboard shortcut reference overlay | Low |
| DXCC entity lookup | Show country name, worked-before status from logbook | Medium |
| DX priority scoring | Factor in DXCC entity, band/mode combos, contest status, propagation | Low |
| Audio level meter | Real-time input level display in station info panel | Medium |
| Device selection UI | Populate and select audio devices from within TUI | Medium |
| DSP/decoder stats | Messages/min, decode latency, processed samples/sec | Low |
| Standalone TUI mode | Full audio→DSP→FT8 pipeline without coordinator | Low |

---

## Success Criteria

1. All existing tests pass after changes
2. Zero dangerous `unwrap()` calls remain in the identified locations
3. `pancetta run --wav <file>` displays correct operating frequency (not hardcoded 14.074)
4. Decoded messages with grid squares show distance and bearing in band activity panel
5. No regressions in clippy or formatting checks
