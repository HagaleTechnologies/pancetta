# Waterfall TX-Offset Redesign

**Date:** 2026-04-29
**Author:** K5ARH (with Claude)
**Status:** Draft — pending review

## Goal

Make the TUI waterfall the operator's primary tool for choosing a clean TX
audio offset. The current waterfall is unreadable because every row is
independently min/max-stretched, so signals never visually "pop." Replace
that with per-bin noise-floor subtraction, a parity-aware occupancy strip,
a frequency axis, and a `T` key that auto-picks the recommended offset
from the existing `SmartFrequencyAllocator`.

## Architecture

Two surfaces change:

1. **DSP row generator** (`pancetta/src/coordinator/dsp.rs`) — replace
   per-row min/max normalization with per-bin rolling-median noise-floor
   subtraction over the last 60 rows. Output rows become *signal-above-
   floor* in absolute terms (0..1 = 0..12 dB above floor).

2. **TUI waterfall widget** (`pancetta-tui/src/widgets/mod.rs`,
   `pancetta-tui/src/ui/mod.rs`) — adds an occupancy strip at the top, a
   frequency axis at the bottom, parity-aware TX-cursor coloring, and a
   `T` key that calls the existing `SmartFrequencyAllocator` and moves
   the cursor to the recommended offset.

The `SmartFrequencyAllocator` (`pancetta-qso/src/frequency.rs:175`) is
reused unchanged. The "best candidate clear in *my* parity" requirement
is satisfied by post-hoc filtering in the `tui_relay` handler — the
allocator already exposes per-slot activity counts via `DecodeHistory`.

## Tech Stack

ratatui (existing), rustfft (existing). No new deps.

## Components

### 1. Per-bin noise floor (DSP)

**File:** `pancetta/src/coordinator/dsp.rs:266-313` (live waterfall block)

State added inside the live-waterfall closure:

```rust
// One sliding window of recent dB powers per bin. 60 entries = 60s at 1 row/s.
let mut bin_history: Vec<std::collections::VecDeque<f32>> =
    vec![std::collections::VecDeque::with_capacity(60); bin_end + 1];
const BIN_HISTORY_LEN: usize = 60;
const NOISE_FLOOR_DB_SCALE: f32 = 12.0; // 12 dB above floor → fully bright
const MIN_HISTORY_FOR_FLOOR: usize = 5;
```

Per-row processing replaces the existing min/max normalization
(`dsp.rs:308-312`):

```rust
// Push current dB powers into per-bin history.
for (i, &p) in powers.iter().enumerate() {
    if bin_history[i].len() >= BIN_HISTORY_LEN {
        bin_history[i].pop_front();
    }
    bin_history[i].push_back(p);
}

// Compute signal-above-floor row.
let row: Vec<f32> = powers
    .iter()
    .enumerate()
    .map(|(i, &p)| {
        if bin_history[i].len() < MIN_HISTORY_FOR_FLOOR {
            // Not enough history yet — fall back to a reasonable default
            // so first ~5s aren't pitch black.
            return 0.0;
        }
        let mut sorted: Vec<f32> = bin_history[i].iter().copied().collect();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let median = sorted[sorted.len() / 2];
        ((p - median).max(0.0) / NOISE_FLOOR_DB_SCALE).clamp(0.0, 1.0)
    })
    .collect();
let _ = live_waterfall_tx.try_send(vec![row]);
```

**Trade-off documented inline:** signals on the air for >60 seconds will
slowly fade as their power becomes part of the median. That's acceptable
for TX-offset choice (we care about *current* activity) and matches
WSJT-X's behavior.

**Cost check:** ~1500 bins × 60-entry sort per second. `sort` on 60 floats
is ~µs; total <2 ms/sec. Negligible.

### 2. Occupancy strip (top row of waterfall)

**File:** `pancetta-tui/src/widgets/mod.rs` (in `Waterfall::render`)

The widget needs the operator's TX-parity and recent decodes. Add fields:

```rust
pub struct Waterfall<'a> {
    // ... existing fields ...
    /// Recent decoded messages with frequency + parity for occupancy display.
    decoded_for_occupancy: &'a [(f64, pancetta_core::SlotParity, chrono::DateTime<chrono::Utc>)],
    /// Operator's TX-parity (resolved from station config / active QSO).
    tx_parity: Option<pancetta_core::SlotParity>,
}
```

Builder methods to set them. In `render`, before drawing data rows,
reserve `waterfall_area.y` (top row) for the occupancy strip and
`waterfall_area.y + height - 1` (bottom row) for the freq axis. Data
rows use `y + 1 .. y + height - 1`.

Occupancy color rule for each column:

```rust
fn occupancy_color(
    col: usize,
    width: usize,
    freq_range: (f64, f64),
    decoded: &[(f64, SlotParity, DateTime<Utc>)],
    tx_parity: Option<SlotParity>,
    now: DateTime<Utc>,
) -> Option<Color> {
    let (lo, hi) = freq_range;
    let bin_hz = (hi - lo) / width as f64;
    let freq_lo = lo + col as f64 * bin_hz;
    let freq_hi = freq_lo + bin_hz;
    let cutoff = now - chrono::Duration::seconds(60); // last 4 cycles

    let in_band = |d: &(f64, SlotParity, DateTime<Utc>)| {
        d.0 >= freq_lo - 37.5 && d.0 <= freq_hi + 37.5 && d.2 >= cutoff
    };

    let busy_own = tx_parity
        .map(|p| decoded.iter().any(|d| in_band(d) && d.1 == p))
        .unwrap_or(false);
    let busy_other = tx_parity
        .map(|p| decoded.iter().any(|d| in_band(d) && d.1 != p))
        .unwrap_or_else(|| decoded.iter().any(in_band));

    if busy_own {
        Some(Color::Red)
    } else if busy_other {
        Some(Color::Yellow)
    } else if !decoded.is_empty() {
        // Only paint green where the band is well-populated to avoid
        // noisy green stripes on a quiet band.
        Some(Color::Green)
    } else {
        None
    }
}
```

Each cell of the strip renders `█` in that color (or `·` for `None`).
`now` is captured once per render call as `chrono::Utc::now()` and
threaded through the helpers — no need for a builder.

### 3. Frequency axis (bottom row)

**File:** `pancetta-tui/src/widgets/mod.rs` (in `Waterfall::render`)

Render one row at `y + height - 1`:

```rust
const TICK_FREQS: &[f64] = &[500.0, 1000.0, 1500.0, 2000.0, 2500.0];
let axis_y = waterfall_area.y + waterfall_area.height - 1;
// Background: dim gray ─
for col in 0..width {
    let x = waterfall_area.x + col as u16;
    buf[(x, axis_y)].set_char('─').set_fg(Color::DarkGray);
}
// Tick marks + labels (only if width >= 40)
for &freq in TICK_FREQS {
    if let Some(col) = self.freq_to_col(freq, width) {
        let x = waterfall_area.x + col as u16;
        buf[(x, axis_y)].set_char('┴').set_fg(Color::Gray);
        if waterfall_area.width >= 40 {
            // Place label centered over the tick when there's room.
            let label = format!("{:.0}", freq);
            let label_x = x.saturating_sub(label.len() as u16 / 2);
            for (i, ch) in label.chars().enumerate() {
                let lx = label_x + i as u16;
                if lx >= waterfall_area.x
                    && lx < waterfall_area.x + waterfall_area.width
                {
                    // Render label one row UP from the axis (so the axis row stays clean).
                    if axis_y > waterfall_area.y {
                        buf[(lx, axis_y - 1)]
                            .set_char(ch)
                            .set_fg(Color::DarkGray);
                    }
                }
            }
        }
    }
}
```

Note: labels render on the row *above* the axis, on top of the lowest data
row. Acceptable cost — the bottom data row is the oldest data anyway.

### 4. TX-cursor live feedback

**File:** `pancetta-tui/src/widgets/mod.rs`

When drawing the TX cursor (existing code at `widgets/mod.rs:264-287`),
look up the column's occupancy color and use it as the cursor color:

```rust
if let Some(tx_hz) = self.tx_offset {
    if let Some(col) = self.freq_to_col(tx_hz, width) {
        let cursor_color = match occupancy_color(col, width, self.freq_range,
                                                  self.decoded_for_occupancy,
                                                  self.tx_parity, now) {
            Some(Color::Red) => Color::Red,
            Some(Color::Yellow) => Color::Yellow,
            _ => Color::Green,
        };
        let label_suffix = match cursor_color {
            Color::Red => " ✗",
            Color::Yellow => " ⚠",
            _ => "",
        };
        // ... existing rendering with these instead of hard-coded green ...
    }
}
```

### 5. `T` key — find clear spot

**File:** `pancetta-tui/src/tui_runner.rs` — add `TuiCommand::FindClearOffset`,
key handler for `T` (uppercase, no shift modifier complication: emit on
Char('t') | Char('T')).

**File:** `pancetta/src/coordinator/tui_relay.rs` — handle `FindClearOffset`:

```rust
TuiCommand::FindClearOffset => {
    let snapshot = build_spectral_snapshot(&latest_waterfall_row,
                                           &bin_medians);
    let history = build_decode_history(&app.decoded_messages);
    let own = current_active_qso_freqs();
    let candidates = allocator.rank_candidates(&snapshot, &history,
                                                &own, None);
    let target_parity = resolve_tx_parity(&config, active_qso);
    let pick = candidates.iter()
        .find(|c| !is_blocked_in_parity(c.offset_hz, &history, target_parity));
    if let Some(c) = pick {
        let _ = bus_tx.send(MessageType::TxFrequencyOffset { offset_hz: c.offset_hz });
        let _ = tui_msg_tx.send(TuiMessage::StatusUpdate {
            component: "waterfall".into(),
            status: format!("TX cursor → {:.0} Hz (clear)", c.offset_hz),
        });
    } else {
        let _ = tui_msg_tx.send(TuiMessage::StatusUpdate {
            component: "waterfall".into(),
            status: "No clear offset found in your parity".into(),
        });
    }
}
```

`MessageType::TxFrequencyOffset` may need to be added to the message bus
if it doesn't already exist; if it does, reuse it. The coordinator
listens for it and updates the shared TX-offset state, which is already
forwarded to the TUI.

The DSP loop emits per-bin medians on a new lightweight channel
(`crossbeam_channel::Sender<Vec<f32>>` of length 1, latest-wins) so
`tui_relay` can build the snapshot without recomputing.

### 6. Layout adjustments

**File:** `pancetta-tui/src/ui/mod.rs:55`

```rust
Constraint::Percentage(40), // Waterfall (was 30)
```

Other left-column percentages must sum to 60. Rebalance band activity /
QSO status accordingly.

**File:** `pancetta-tui/src/widgets/mod.rs:236-262` — delete the E/O cycle
marker block. Parity is now visually conveyed by the occupancy strip.

## Data Flow

```
audio → DSP → live_wf_fft
              ├── per-bin median compute (NEW, in-place)
              ├── waterfall_row (signal-above-floor) → waterfall_tx
              └── bin_medians snapshot                → bin_medians_tx (NEW)

decoded messages → coordinator → tui_relay → app.decoded_messages
                                              ↓
                              filter by parity + timestamp (last 60s)
                                              ↓
                              passed into Waterfall widget for occupancy

T key → TuiCommand::FindClearOffset → tui_relay
        (uses latest waterfall_row + bin_medians + decoded_messages)
        → SmartFrequencyAllocator::rank_candidates
        → filter by target_parity
        → MessageType::TxFrequencyOffset → app.tx_frequency_offset
```

## Error Handling

This is UI; failures are non-fatal:

- DSP bin history underfilled (<5 rows) → emit zeros for those bins;
  waterfall starts gray, fills in within 5 seconds.
- Allocator returns no candidates → status bar reports "No clear offset
  found in your parity"; cursor stays put.
- `tx_parity` is `None` (idle, no QSO, parity = Auto) → occupancy strip
  shows yellow for any recent decode, no red zone. Cursor stays green.

## Testing

### Unit tests

- **`dsp.rs::rolling_median_per_bin`** — pure function extraction.
  Given 60 dB samples per bin, returns expected median. Test edge cases:
  empty history, single sample, monotonic ramp.
- **`widgets/mod.rs::occupancy_color`** — pure function. Test:
  - All decoded in own parity → red
  - All decoded in other parity → yellow
  - No decoded in band → green (when band is otherwise populated)
  - No decoded anywhere → None
  - Decoded outside ±37.5 Hz of column → not counted
  - Decoded older than 60s → not counted
- **`widgets/mod.rs::freq_axis_tick_alignment`** — given width=80,
  freq_range=(0,3000), tick at 1500 Hz lands at column 40.
- **`frequency.rs::rank_candidates`** — existing tests pass unchanged.

### Integration

- `pancetta/tests/waterfall_redesign.rs` (new): drive the coordinator
  with synthetic `WaterfallUpdate` + `DecodedMessage` events, send
  `TuiCommand::FindClearOffset`, assert `app.tx_frequency_offset`
  changes to a value the allocator would pick.

### Manual (Phase 5)

Operator validates on a real band:

1. Tune to a busy band. Observe whether occupancy strip matches what
   `Band Activity` says is being decoded.
2. Move TX cursor manually with `/` — does it turn red over decoded
   signal columns?
3. Press `T` — does it land on a green/clear column?
4. Watch a known-strong signal: does it visibly stand out compared to
   the noise floor? (Pre-redesign: it didn't.)

## Out of Scope (Deferred)

- Persistent decode-callsign labels overlaid on waterfall (option B
  from the design conversation; nice-to-have, defer).
- Replacing waterfall with band-map widget (option C; this redesign
  brings the waterfall to par, eliminating the case for replacement).
- Splitting into per-parity sub-waterfalls (occupancy strip captures
  parity info more compactly).
- Renaming `T` — pending the broader keyboard-input rethink already
  queued. Document `T` in the runbook for now.
