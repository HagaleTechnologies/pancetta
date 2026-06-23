# Waterfall TX-Offset Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace per-row min/max waterfall normalization with per-bin noise-floor subtraction and add parity-aware occupancy + auto-pick so the operator can choose a clean TX audio offset at a glance.

**Architecture:** DSP emits *signal-above-floor* (0..1 = 0..12 dB above per-bin median over 60s) instead of per-row min/max. The TUI waterfall widget renders an occupancy strip (top), a frequency axis (bottom), and colors the TX cursor by occupancy under the column. A new `T` key invokes a TUI-local clear-offset finder that scores candidates from the latest spectral row + recent decodes; no new message bus types or cross-crate deps.

**Tech Stack:** ratatui (existing), rustfft (existing), pancetta-core::SlotParity. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-04-29-waterfall-tx-offset-design.md`

---

## File Map

**New files:**
- None (all changes go into existing files).

**Modify:**
- `pancetta/src/coordinator/dsp.rs` — replace per-row min/max with per-bin rolling-median noise-floor subtraction.
- `pancetta-tui/src/widgets/mod.rs` — add occupancy strip, frequency axis, parity-aware cursor; drop E/O markers.
- `pancetta-tui/src/ui/mod.rs` — wire decoded messages + parity into widget; bump waterfall height 30→40%.
- `pancetta-tui/src/app.rs` — add `find_clear_offset()` method and `tx_self_parity` field.
- `pancetta-tui/src/tui_runner.rs` — add `TuiCommand::FindClearOffset`, `T` key handler.
- `pancetta/src/coordinator/tui_relay.rs` — forward `FindClearOffset` (no-op pass-through to App).
- `docs/RUNBOOK.md` and `README.md` — document `T` key.

---

### Task 1: Extract rolling-median helper in DSP

**Files:**
- Modify: `pancetta/src/coordinator/dsp.rs`

- [ ] **Step 1: Write failing tests for `rolling_median`**

Add to bottom of `pancetta/src/coordinator/dsp.rs` (inside or above existing `#[cfg(test)] mod tests` if present, else create one):

```rust
#[cfg(test)]
mod median_tests {
    use super::rolling_median;

    #[test]
    fn empty_returns_zero() {
        assert_eq!(rolling_median(&[]), 0.0);
    }

    #[test]
    fn single_value_is_itself() {
        assert_eq!(rolling_median(&[7.0]), 7.0);
    }

    #[test]
    fn odd_length_picks_middle() {
        assert_eq!(rolling_median(&[1.0, 5.0, 3.0]), 3.0);
    }

    #[test]
    fn even_length_picks_upper_middle() {
        // For waterfall use (noise-floor estimation), upper-middle is fine —
        // we don't need the strict midpoint average.
        assert_eq!(rolling_median(&[1.0, 2.0, 3.0, 4.0]), 3.0);
    }

    #[test]
    fn ignores_nan() {
        // partial_cmp returns None for NaN; we use unwrap_or(Equal). Just
        // verify it doesn't panic.
        let xs = [1.0, f32::NAN, 3.0, 2.0];
        let m = rolling_median(&xs);
        assert!(m.is_finite());
    }
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test -p pancetta --lib median_tests 2>&1 | tail -30`
Expected: compile error — `rolling_median` not found.

- [ ] **Step 3: Implement `rolling_median`**

Add the helper to `pancetta/src/coordinator/dsp.rs` (top-level, near the other helpers):

```rust
/// Rolling median over a recent window of dB powers. Used as a per-bin
/// noise-floor estimate so the waterfall renders signal-above-floor
/// instead of per-row min/max stretch (which hid signals at all amplitudes).
fn rolling_median(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<f32> = samples.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    sorted[sorted.len() / 2]
}
```

- [ ] **Step 4: Run the test to confirm it passes**

Run: `cargo test -p pancetta --lib median_tests 2>&1 | tail -20`
Expected: `5 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/dsp.rs
git commit -m "feat(dsp): rolling_median helper for per-bin noise floor

Pure helper extracted ahead of integration so it's unit-testable.
Returns the upper-middle element for even-length inputs (good enough
for noise-floor estimation; no need for true midpoint average).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Per-bin noise floor in live waterfall

**Files:**
- Modify: `pancetta/src/coordinator/dsp.rs:266-313` (the live-waterfall block)

- [ ] **Step 1: Locate and read the live-waterfall block**

Open `pancetta/src/coordinator/dsp.rs` and find the block starting at the comment `// Live waterfall: emit one spectrum row per second using rustfft.` (around line 266). The current code at 308-312 normalizes per-row using min/max:

```rust
let min_p = powers.iter().cloned().fold(f32::MAX, f32::min);
let max_p = powers.iter().cloned().fold(f32::MIN, f32::max);
let range = (max_p - min_p).max(1.0);
let row: Vec<f32> =
    powers.iter().map(|&p| (p - min_p) / range).collect();
let _ = live_waterfall_tx.try_send(vec![row]);
```

- [ ] **Step 2: Add per-bin history state above the live-waterfall closure**

Find where `last_live_wf_samples` is initialized (just before the loop that owns the live-waterfall block) and add alongside it:

```rust
const BIN_HISTORY_LEN: usize = 60;
const NOISE_FLOOR_DB_SCALE: f32 = 12.0;
const MIN_HISTORY_FOR_FLOOR: usize = 5;
let mut bin_history: Vec<std::collections::VecDeque<f32>> = Vec::new();
```

If `last_live_wf_samples` is declared inside a `move` closure, declare `bin_history` in the same scope. (Search for `let mut last_live_wf_samples` — that's the right spot.)

- [ ] **Step 3: Replace the per-row min/max with per-bin median subtraction**

Replace the four lines at the original 308-312 (the `min_p`/`max_p`/`range`/`row` block) with:

```rust
// Lazy-init history with the right number of bins on the first row
// so we don't have to know `bin_end + 1` before the FFT runs.
if bin_history.len() != powers.len() {
    bin_history = (0..powers.len())
        .map(|_| std::collections::VecDeque::with_capacity(BIN_HISTORY_LEN))
        .collect();
}

// Push current dB powers into per-bin history (drop oldest if full).
for (i, &p) in powers.iter().enumerate() {
    if bin_history[i].len() >= BIN_HISTORY_LEN {
        bin_history[i].pop_front();
    }
    bin_history[i].push_back(p);
}

// Output row: signal-above-floor in 0..1 (0..NOISE_FLOOR_DB_SCALE dB above
// the rolling per-bin median). Until each bin has MIN_HISTORY_FOR_FLOOR
// samples, emit zero so the waterfall starts dim instead of with garbage.
let row: Vec<f32> = powers
    .iter()
    .enumerate()
    .map(|(i, &p)| {
        if bin_history[i].len() < MIN_HISTORY_FOR_FLOOR {
            return 0.0;
        }
        let history: Vec<f32> = bin_history[i].iter().copied().collect();
        let median = rolling_median(&history);
        ((p - median).max(0.0) / NOISE_FLOOR_DB_SCALE).clamp(0.0, 1.0)
    })
    .collect();
let _ = live_waterfall_tx.try_send(vec![row]);
```

- [ ] **Step 4: Build to verify**

Run: `cargo build -p pancetta 2>&1 | tail -15`
Expected: clean build.

- [ ] **Step 5: Run pancetta workspace tests to confirm no regression**

Run: `cargo test -p pancetta --lib 2>&1 | tail -15`
Expected: all existing tests still pass; new `median_tests` group passes too.

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/dsp.rs
git commit -m "feat(dsp): per-bin rolling-median noise floor for waterfall

Replaces per-row min/max stretch with signal-above-floor (0..12 dB
above per-bin 60s median). Strong signals now visually pop; idle
slots stay dim. First ~5 seconds render as zeros while the per-bin
history fills.

Cost: ~1500 bins x 60-element sort/sec = sub-millisecond.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Add `tx_self_parity` to App and resolve from config + active QSO

**Files:**
- Modify: `pancetta-tui/src/app.rs` (around the existing TX-state fields, ~360)

- [ ] **Step 1: Write failing tests for parity resolution**

Add to the existing `#[cfg(test)] mod tests` in `pancetta-tui/src/app.rs`:

```rust
#[tokio::test]
async fn resolves_parity_from_active_qso_when_present() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.active_qsos = vec![ActiveQsoBanner {
        their_callsign: "W1AW".into(),
        state: "Calling".into(),
        started_at: chrono::Utc::now(),
        frequency_hz: 1234.0,
        tx_parity: Some(pancetta_core::slot::SlotParity::Even),
    }];
    assert_eq!(app.resolve_tx_parity(), Some(pancetta_core::slot::SlotParity::Even));
}

#[tokio::test]
async fn resolves_parity_from_config_when_idle() {
    let mut config = Config::default();
    config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;
    let app = App::new(config, None).await.unwrap();
    assert_eq!(app.resolve_tx_parity(), Some(pancetta_core::slot::SlotParity::Even));
}

#[tokio::test]
async fn resolves_none_when_auto_and_idle() {
    let app = App::new(Config::default(), None).await.unwrap();
    // Default tx_self_parity is Auto.
    assert_eq!(app.resolve_tx_parity(), None);
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test -p pancetta-tui --lib resolves_parity 2>&1 | tail -20`
Expected: compile error — `tx_parity` field on `ActiveQsoBanner` and/or `resolve_tx_parity` method missing.

- [ ] **Step 3: Thread `tx_parity` through bus → banner**

`ActiveQsoSnapshotItem` (`pancetta/src/message_bus.rs:202`) currently has
fields `their_callsign`, `state`, `started_at`, `frequency_hz`. Add a new
field at the bottom of the struct:

```rust
/// Parity our station transmits in for this QSO. Used by the TUI
/// waterfall to color the occupancy strip and TX cursor by "is this
/// slot mine."
pub tx_parity: Option<pancetta_core::slot::SlotParity>,
```

`ActiveQsoBanner` (`pancetta-tui/src/app.rs:79`) has the same field names
plus the parity. Add at the bottom of the struct:

```rust
/// Parity our station transmits in for this QSO. None when unknown.
pub tx_parity: Option<pancetta_core::slot::SlotParity>,
```

In `pancetta/src/coordinator/qso.rs:608` (`build_active_qso_snapshot`),
find the place where `ActiveQsoSnapshotItem` is constructed (line ~641).
Populate `tx_parity` from the QSO's `metadata.tx_parity` field. If the
construction looks like:

```rust
Some(crate::message_bus::ActiveQsoSnapshotItem {
    their_callsign: ...,
    state: ...,
    started_at: ...,
    frequency_hz: ...,
})
```

add `tx_parity: qso.metadata.tx_parity,` (use whatever path actually
holds the parity in the QSO state — `QsoMetadata.tx_parity` is the
canonical field per CLAUDE.md).

In `pancetta/src/coordinator/tui_relay.rs:192` where `ActiveQsosSnapshot`
is unpacked into `ActiveQsoBanner` instances, forward the new field.
Pattern: `tx_parity: item.tx_parity,`.

- [ ] **Step 4: Add `resolve_tx_parity` method on App**

In `pancetta-tui/src/app.rs`, add to the `impl App` block:

```rust
/// The parity our station will TX in. Active QSO wins; otherwise fall
/// back to config (Even/Odd) or None for Auto.
pub fn resolve_tx_parity(&self) -> Option<pancetta_core::slot::SlotParity> {
    if let Some(qso) = self.active_qsos.first() {
        if let Some(p) = qso.tx_parity {
            return Some(p);
        }
    }
    match self.config.station.tx_self_parity {
        pancetta_config::station::TxSelfParity::Even => Some(pancetta_core::slot::SlotParity::Even),
        pancetta_config::station::TxSelfParity::Odd => Some(pancetta_core::slot::SlotParity::Odd),
        pancetta_config::station::TxSelfParity::Auto => None,
    }
}
```

`App` already has `pub config: Config` (`pancetta-tui/src/app.rs:316`),
so no additional plumbing is needed.

- [ ] **Step 5: Run the test to confirm it passes**

Run: `cargo test -p pancetta-tui --lib resolves_parity 2>&1 | tail -20`
Expected: `3 passed; 0 failed`.

- [ ] **Step 6: Run the full TUI test suite to confirm no regression**

Run: `cargo test -p pancetta-tui --lib 2>&1 | tail -15`
Expected: all existing tests still pass.

- [ ] **Step 7: Commit**

```bash
git add pancetta-tui/src/app.rs pancetta/src/message_bus.rs pancetta/src/coordinator/qso.rs
git commit -m "feat(tui): resolve_tx_parity — active QSO > config > Auto

Threads parity from QsoMetadata through ActiveQsoSnapshotItem into the
TUI banner so the waterfall widget can color the occupancy strip and
TX cursor by 'is this slot mine'.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Pure `occupancy_color` helper for the widget

**Files:**
- Modify: `pancetta-tui/src/widgets/mod.rs`

- [ ] **Step 1: Write failing tests for occupancy_color**

Add to the bottom of `pancetta-tui/src/widgets/mod.rs` `#[cfg(test)] mod tests`:

```rust
#[test]
fn occupancy_red_when_decode_in_own_parity() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;
    let now = Utc::now();
    let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(10))];
    let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
    assert_eq!(c, Some(Color::Red));
}

#[test]
fn occupancy_yellow_when_decode_in_other_parity_only() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;
    let now = Utc::now();
    let decoded = vec![(1500.0, SlotParity::Odd, now - Duration::seconds(10))];
    let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
    assert_eq!(c, Some(Color::Yellow));
}

#[test]
fn occupancy_drops_decodes_older_than_60s() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;
    let now = Utc::now();
    let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(120))];
    let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
    // 60+s old decode should be excluded; column has no relevant decodes.
    // With an empty band-overall (all decodes filtered), returns None.
    assert_eq!(c, None);
}

#[test]
fn occupancy_drops_decodes_outside_band() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;
    let now = Utc::now();
    let decoded = vec![(2000.0, SlotParity::Even, now - Duration::seconds(10))];
    // Column 40 in width 80 over (0,3000) = 1500 Hz ± 18.75. 2000 is well outside.
    let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, Some(SlotParity::Even), now);
    // Decode exists but not in this column's band — column itself has
    // nothing nearby in either parity → None.
    assert_eq!(c, None);
}

#[test]
fn occupancy_yellow_when_no_tx_parity_known() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;
    let now = Utc::now();
    let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(10))];
    // tx_parity = None means we can't say "your" vs "their" — collapse to yellow.
    let c = occupancy_color(40, 80, (0.0, 3000.0), &decoded, None, now);
    assert_eq!(c, Some(Color::Yellow));
}
```

- [ ] **Step 2: Run the tests to confirm they fail**

Run: `cargo test -p pancetta-tui --lib occupancy 2>&1 | tail -20`
Expected: compile error — `occupancy_color` not defined.

- [ ] **Step 3: Implement `occupancy_color`**

In `pancetta-tui/src/widgets/mod.rs`, above the `Waterfall` struct, add:

```rust
/// Color a column of the occupancy strip by recent decode activity.
/// Red = decode within ±37.5 Hz in YOUR TX parity in the last 60s.
/// Yellow = decode within ±37.5 Hz in the OTHER parity (or own-parity unknown).
/// None = column is clear (no decodes nearby in the last 60s).
///
/// `decoded` is `(frequency_hz, parity, timestamp)`. `tx_parity = None`
/// (operator's parity unknown — Auto + idle) collapses red→yellow because
/// we can't say if a decode would collide.
pub(crate) fn occupancy_color(
    col: usize,
    width: usize,
    freq_range: (f64, f64),
    decoded: &[(f64, pancetta_core::slot::SlotParity, chrono::DateTime<chrono::Utc>)],
    tx_parity: Option<pancetta_core::slot::SlotParity>,
    now: chrono::DateTime<chrono::Utc>,
) -> Option<ratatui::style::Color> {
    if width == 0 {
        return None;
    }
    let (lo, hi) = freq_range;
    let bin_hz = (hi - lo) / width as f64;
    let center = lo + (col as f64 + 0.5) * bin_hz;
    let cutoff = now - chrono::Duration::seconds(60);

    let in_band = |d: &(f64, pancetta_core::slot::SlotParity, chrono::DateTime<chrono::Utc>)| {
        (d.0 - center).abs() <= 37.5 && d.2 >= cutoff
    };

    let any_in_band = decoded.iter().any(in_band);
    if !any_in_band {
        return None;
    }

    match tx_parity {
        Some(my_parity) => {
            let busy_own = decoded.iter().any(|d| in_band(d) && d.1 == my_parity);
            if busy_own {
                Some(ratatui::style::Color::Red)
            } else {
                Some(ratatui::style::Color::Yellow)
            }
        }
        None => Some(ratatui::style::Color::Yellow),
    }
}
```

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test -p pancetta-tui --lib occupancy 2>&1 | tail -20`
Expected: `5 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/widgets/mod.rs
git commit -m "feat(tui): occupancy_color pure helper for waterfall strip

Computes red/yellow/None per column from recent decodes + operator's
TX parity. Pure function so the rendering code stays small and the
behavior is unit-testable without a fake terminal.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Render occupancy strip + frequency axis in Waterfall widget

**Files:**
- Modify: `pancetta-tui/src/widgets/mod.rs`

- [ ] **Step 1: Add fields + builders to `Waterfall`**

Add to the `Waterfall<'a>` struct in `pancetta-tui/src/widgets/mod.rs:11`:

```rust
/// Recent decodes (frequency, parity, timestamp) for the occupancy strip.
decoded_for_occupancy: &'a [(f64, pancetta_core::slot::SlotParity, chrono::DateTime<chrono::Utc>)],
/// Operator's resolved TX parity (None when Auto + idle).
tx_parity: Option<pancetta_core::slot::SlotParity>,
```

Update `Waterfall::new` to default these to `&[]` and `None`. Add builder methods:

```rust
pub fn decoded_for_occupancy(
    mut self,
    decoded: &'a [(f64, pancetta_core::slot::SlotParity, chrono::DateTime<chrono::Utc>)],
) -> Self {
    self.decoded_for_occupancy = decoded;
    self
}

pub fn tx_parity(mut self, parity: Option<pancetta_core::slot::SlotParity>) -> Self {
    self.tx_parity = parity;
    self
}
```

`Waterfall::new` initialization needs the new fields too:

```rust
decoded_for_occupancy: &[],
tx_parity: None,
```

- [ ] **Step 2: Build to confirm fields wire up**

Run: `cargo build -p pancetta-tui 2>&1 | tail -10`
Expected: clean (existing call sites still compile because the new fields default).

- [ ] **Step 3: Reserve top + bottom rows in `render`**

In `Waterfall::render` (around `widgets/mod.rs:170`), modify the data-rendering loop to skip the top row (occupancy strip) and bottom row (axis). Replace the existing `for display_row in 0..rows_to_show` with logic that draws data rows in `y + 1 .. y + height - 1`:

Find:

```rust
let rows_to_show = height.min(effective_rows);
for display_row in 0..rows_to_show {
    let y = waterfall_area.y + display_row as u16;
```

Change to:

```rust
// Reserve top row for occupancy strip and bottom row for freq axis.
let data_rows_available = height.saturating_sub(2);
let rows_to_show = data_rows_available.min(effective_rows);
for display_row in 0..rows_to_show {
    let y = waterfall_area.y + 1 + display_row as u16;
```

- [ ] **Step 4: Render the occupancy strip**

After the data-rendering loop in `render`, add (replacing the deleted E/O cycle-marker block from Task 6):

```rust
// Occupancy strip: top row of the waterfall.
let now = chrono::Utc::now();
for col in 0..width {
    let x = waterfall_area.x + col as u16;
    let strip_y = waterfall_area.y;
    if let Some(color) = occupancy_color(
        col,
        width,
        self.freq_range,
        self.decoded_for_occupancy,
        self.tx_parity,
        now,
    ) {
        buf[(x, strip_y)].set_char('█').set_fg(color);
    } else {
        buf[(x, strip_y)]
            .set_char('·')
            .set_fg(ratatui::style::Color::DarkGray);
    }
}
```

- [ ] **Step 5: Render the frequency axis**

Below the occupancy strip block:

```rust
// Frequency axis: bottom row.
const TICK_FREQS: &[f64] = &[500.0, 1000.0, 1500.0, 2000.0, 2500.0];
let axis_y = waterfall_area.y + waterfall_area.height - 1;
for col in 0..width {
    let x = waterfall_area.x + col as u16;
    buf[(x, axis_y)]
        .set_char('─')
        .set_fg(ratatui::style::Color::DarkGray);
}
for &freq in TICK_FREQS {
    if let Some(col) = self.freq_to_col(freq, width) {
        let x = waterfall_area.x + col as u16;
        buf[(x, axis_y)]
            .set_char('┴')
            .set_fg(ratatui::style::Color::Gray);
        if waterfall_area.width >= 40 && axis_y > waterfall_area.y + 1 {
            let label = format!("{:.0}", freq);
            let label_x = x.saturating_sub((label.len() / 2) as u16);
            for (i, ch) in label.chars().enumerate() {
                let lx = label_x + i as u16;
                if lx < waterfall_area.x + waterfall_area.width {
                    buf[(lx, axis_y - 1)]
                        .set_char(ch)
                        .set_fg(ratatui::style::Color::DarkGray);
                }
            }
        }
    }
}
```

- [ ] **Step 6: Add a render-buffer test**

Add to the same test module:

```rust
#[test]
fn waterfall_renders_occupancy_strip_top_row() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    let now = Utc::now();
    let data: Vec<Vec<f32>> = vec![vec![0.5; 64]; 30];
    let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(5))];
    let area = Rect::new(0, 0, 80, 10);
    let mut buf = Buffer::empty(area);

    Waterfall::new(&data)
        .decoded_for_occupancy(&decoded)
        .tx_parity(Some(SlotParity::Even))
        .render(area, &mut buf);

    // The column for 1500 Hz over (0,3000) and width 80 is column 40.
    // Top row should show '█' colored Red because decode is in our parity.
    let cell = &buf[(40, 0)];
    assert_eq!(cell.symbol(), "█");
    assert_eq!(cell.fg, ratatui::style::Color::Red);
}

#[test]
fn waterfall_renders_freq_axis_bottom_row() {
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    let data: Vec<Vec<f32>> = vec![vec![0.0; 64]; 10];
    let area = Rect::new(0, 0, 80, 10);
    let mut buf = Buffer::empty(area);

    Waterfall::new(&data).render(area, &mut buf);

    // 1500 Hz tick at column 40 in the bottom row.
    let tick = &buf[(40, 9)];
    assert_eq!(tick.symbol(), "┴");
}
```

- [ ] **Step 7: Run the tests to confirm they pass**

Run: `cargo test -p pancetta-tui --lib widgets 2>&1 | tail -20`
Expected: all widget tests pass including the two new ones.

- [ ] **Step 8: Commit**

```bash
git add pancetta-tui/src/widgets/mod.rs
git commit -m "feat(tui): occupancy strip + frequency axis on waterfall

Top row: parity-aware occupancy column (red = busy in your parity,
yellow = busy in other parity, dim · = clear). Bottom row: tick marks
labeled at 500/1000/1500/2000/2500 Hz. Data rows shrink to y+1..y+h-1.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 6: Parity-aware TX cursor and remove E/O markers

**Files:**
- Modify: `pancetta-tui/src/widgets/mod.rs`

- [ ] **Step 1: Color the TX cursor by occupancy under the column**

Find the TX-cursor block in `Waterfall::render` (around line 264):

```rust
if let Some(tx_hz) = self.tx_offset {
    if let Some(col) = self.freq_to_col(tx_hz, width) {
        let x = waterfall_area.x + col as u16;
        for row in 0..height {
            let y = waterfall_area.y + row as u16;
            buf[(x, y)]
                .set_char('│')
                .set_fg(Color::Green)
                .set_bg(Color::Black);
        }
        // Label at top
        let label = format!("TX {:.0}", tx_hz);
        for (i, ch) in label.chars().enumerate() {
            let lx = x.saturating_add(1) + i as u16;
            if lx < waterfall_area.x + waterfall_area.width {
                buf[(lx, waterfall_area.y)]
                    .set_char(ch)
                    .set_fg(Color::Green)
                    .set_bg(Color::Black);
            }
        }
    }
}
```

Replace with parity-aware version (`now` is already in scope from the occupancy strip):

```rust
if let Some(tx_hz) = self.tx_offset {
    if let Some(col) = self.freq_to_col(tx_hz, width) {
        let x = waterfall_area.x + col as u16;
        let cursor_color = match occupancy_color(
            col,
            width,
            self.freq_range,
            self.decoded_for_occupancy,
            self.tx_parity,
            now,
        ) {
            Some(Color::Red) => Color::Red,
            Some(Color::Yellow) => Color::Yellow,
            _ => Color::Green,
        };
        let suffix = match cursor_color {
            Color::Red => " ✗",
            Color::Yellow => " ⚠",
            _ => "",
        };
        for row in 0..height {
            let y = waterfall_area.y + row as u16;
            buf[(x, y)]
                .set_char('│')
                .set_fg(cursor_color)
                .set_bg(Color::Black);
        }
        let label = format!("TX {:.0}{}", tx_hz, suffix);
        for (i, ch) in label.chars().enumerate() {
            let lx = x.saturating_add(1) + i as u16;
            if lx < waterfall_area.x + waterfall_area.width {
                buf[(lx, waterfall_area.y)]
                    .set_char(ch)
                    .set_fg(cursor_color)
                    .set_bg(Color::Black);
            }
        }
    }
}
```

Note: the cursor renders over the full column height including the occupancy-strip row (y) and the axis row (y+h-1). That's intentional — the TX cursor must remain visible at all rows so the operator can see the column even when it overlaps the strip glyph. Within the occupancy strip and axis rows, the cursor still wins because we draw it last.

- [ ] **Step 2: Delete the E/O cycle-marker block**

Find and remove the entire block (around `widgets/mod.rs:236-262`) starting with the comment `// Overlay: cycle boundary markers (dim horizontal ticks on left edge)` and ending with the closing braces of its outer `if`. Parity is now visually conveyed by the occupancy strip; the markers occluded data column 0 and the operator gained nothing.

- [ ] **Step 3: Add a parity-aware-cursor test**

Add to the test module:

```rust
#[test]
fn cursor_red_when_offset_overlaps_busy_own_parity() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::widgets::Widget;

    let now = Utc::now();
    let data: Vec<Vec<f32>> = vec![vec![0.0; 64]; 10];
    let decoded = vec![(1500.0, SlotParity::Even, now - Duration::seconds(5))];
    let area = Rect::new(0, 0, 80, 10);
    let mut buf = Buffer::empty(area);

    Waterfall::new(&data)
        .tx_offset(1500.0)
        .decoded_for_occupancy(&decoded)
        .tx_parity(Some(SlotParity::Even))
        .render(area, &mut buf);

    // Cursor at col 40, mid-height. Red because busy in own parity.
    let cell = &buf[(40, 5)];
    assert_eq!(cell.symbol(), "│");
    assert_eq!(cell.fg, Color::Red);
}
```

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test -p pancetta-tui --lib widgets 2>&1 | tail -20`
Expected: all widget tests pass.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/widgets/mod.rs
git commit -m "feat(tui): parity-aware TX cursor + drop E/O markers

Cursor color now matches occupancy under the column — red ✗ for busy
in your parity, yellow ⚠ for opposite, green when clear. Removes the
E/O cycle markers that were occluding column 0 (parity is now obvious
from the occupancy strip).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 7: Wire decoded messages + parity into widget; bump waterfall height

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs:55, 313-338`

- [ ] **Step 1: Bump waterfall height**

The current left-column layout (`pancetta-tui/src/ui/mod.rs:50-57`) is:

```rust
.constraints([
    Constraint::Length(1),       // Active-QSOs banner
    Constraint::Percentage(45),  // Band activity
    Constraint::Percentage(30),  // Waterfall
    Constraint::Percentage(25),  // QSO status
])
```

Change the Waterfall to 40 and absorb the 10 from Band activity:

```rust
.constraints([
    Constraint::Length(1),       // Active-QSOs banner
    Constraint::Percentage(35),  // Band activity (was 45)
    Constraint::Percentage(40),  // Waterfall (was 30)
    Constraint::Percentage(25),  // QSO status
])
```

- [ ] **Step 2: Pass decoded messages and parity to the widget**

In `render_waterfall` (`pancetta-tui/src/ui/mod.rs:313`):

Replace:

```rust
let waterfall = Waterfall::new(&app.waterfall_data)
    .block(waterfall_block)
    .tx_offset(app.tx_frequency_offset)
    .signal_freqs(signal_freqs)
    .color_capability(app.color_capability);
f.render_widget(waterfall, area);
```

With:

```rust
// Build (freq, parity, timestamp) tuples for the occupancy strip from
// recent decodes. Filter to last 60s; the widget further trims by column.
let cutoff = chrono::Utc::now() - chrono::Duration::seconds(60);
let decoded_for_occupancy: Vec<(
    f64,
    pancetta_core::slot::SlotParity,
    chrono::DateTime<chrono::Utc>,
)> = app
    .decoded_messages
    .iter()
    .filter(|m| m.timestamp >= cutoff)
    .filter_map(|m| {
        m.slot_parity
            .map(|p| (m.delta_freq as f64, p, m.timestamp))
    })
    .collect();
let tx_parity = app.resolve_tx_parity();

let waterfall = Waterfall::new(&app.waterfall_data)
    .block(waterfall_block)
    .tx_offset(app.tx_frequency_offset)
    .signal_freqs(signal_freqs)
    .color_capability(app.color_capability)
    .decoded_for_occupancy(&decoded_for_occupancy)
    .tx_parity(tx_parity);
f.render_widget(waterfall, area);
```

- [ ] **Step 3: Build and run the TUI test suite**

Run: `cargo build -p pancetta-tui 2>&1 | tail -10 && cargo test -p pancetta-tui --lib 2>&1 | tail -15`
Expected: clean build; all tests pass.

- [ ] **Step 4: Commit**

```bash
git add pancetta-tui/src/ui/mod.rs
git commit -m "feat(tui): pass decodes + parity to waterfall; 30%->40% height

The waterfall is now the operator's primary tool for picking a clean
TX offset, so it gets more vertical real estate. Wires decoded messages
and resolved TX-parity into the widget so the occupancy strip + cursor
coloring have data to work with.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 8: TUI-local clear-offset finder

**Files:**
- Modify: `pancetta-tui/src/app.rs`

- [ ] **Step 1: Write failing tests for `find_clear_offset`**

Add to the existing tests in `pancetta-tui/src/app.rs`:

```rust
#[tokio::test]
async fn find_clear_offset_avoids_busy_parity() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;

    let mut app = App::new(Config::default(), None).await.unwrap();
    // Set TX parity so the finder knows what to avoid.
    app.config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;

    // Saturate the band 1400-1600 Hz with Even-parity decodes (busy for us).
    let now = Utc::now();
    for f in (1400..1600).step_by(50) {
        app.decoded_messages.push_back(fixture_view_at(
            "AB1CD",
            f as f32,
            SlotParity::Even,
            now - Duration::seconds(5),
        ));
    }

    // Latest waterfall row is mostly quiet except 1400-1600.
    let mut row = vec![0.0f32; 100];
    for i in 47..54 {
        row[i] = 1.0;
    }
    app.waterfall_data.push(row);

    let pick = app.find_clear_offset().expect("should find a clear spot");
    // Should land outside 1400-1600 ± 75 Hz separation.
    assert!(
        pick < 1325.0 || pick > 1675.0,
        "picked {} which is too close to busy band",
        pick
    );
    // Should be in the allowed range (200..2800).
    assert!(pick >= 200.0 && pick <= 2800.0);
}

#[tokio::test]
async fn find_clear_offset_returns_none_when_band_saturated() {
    use chrono::{Duration, Utc};
    use pancetta_core::slot::SlotParity;

    let mut app = App::new(Config::default(), None).await.unwrap();
    app.config.station.tx_self_parity = pancetta_config::station::TxSelfParity::Even;

    // Decode every 25 Hz across the whole 200-2800 range in our parity.
    let now = Utc::now();
    for f in (200..=2800).step_by(25) {
        app.decoded_messages.push_back(fixture_view_at(
            "ZZZZZ",
            f as f32,
            SlotParity::Even,
            now - Duration::seconds(5),
        ));
    }

    let pick = app.find_clear_offset();
    assert!(pick.is_none(), "should refuse to pick when nothing is clear");
}

// Helper for the tests above.
fn fixture_view_at(
    call: &str,
    delta_freq: f32,
    parity: pancetta_core::slot::SlotParity,
    timestamp: chrono::DateTime<chrono::Utc>,
) -> DecodedMessageView {
    let mut v = fixture_view(call, -10);
    v.delta_freq = delta_freq;
    v.slot_parity = Some(parity);
    v.timestamp = timestamp;
    v
}
```

- [ ] **Step 2: Run the test to confirm it fails**

Run: `cargo test -p pancetta-tui --lib find_clear_offset 2>&1 | tail -20`
Expected: compile error — `find_clear_offset` not defined.

- [ ] **Step 3: Implement `find_clear_offset`**

Add to `impl App` in `pancetta-tui/src/app.rs`:

```rust
/// Find the best TX audio offset given current spectral activity and
/// recent decode history. Returns `None` if every candidate is occupied
/// in our parity. Caller updates `tx_frequency_offset` if Some.
///
/// Scoring (lower = better):
///   spectral_penalty = peak amplitude in latest waterfall row near offset
///   decode_penalty   = N decodes within 37.5 Hz in our parity (last 60s)
///   own_penalty      = 1.0 if any active QSO within 75 Hz else 0.0
///   center_bias      = (|offset - 1500| / 1300) * 0.3   (small)
///
/// Hard reject: any candidate with decode_penalty > 0 or own_penalty > 0.
/// Among the remaining, lowest spectral + center_bias wins.
pub fn find_clear_offset(&self) -> Option<f64> {
    use pancetta_core::slot::SlotParity;

    const MIN_HZ: f64 = 200.0;
    const MAX_HZ: f64 = 2800.0;
    const STEP_HZ: f64 = 25.0;
    const SEPARATION_HZ: f64 = 75.0;
    const NEIGHBOR_HZ: f64 = 37.5;
    const SPECTRAL_RANGE_HZ: (f64, f64) = (0.0, 3000.0);

    let tx_parity: Option<SlotParity> = self.resolve_tx_parity();
    let now = chrono::Utc::now();
    let cutoff = now - chrono::Duration::seconds(60);

    let latest_row: Option<&Vec<f32>> = self.waterfall_data.last();
    let row_len = latest_row.map(|r| r.len()).unwrap_or(0);

    let own_freqs: Vec<f64> = self
        .active_qsos
        .iter()
        .map(|q| q.frequency_hz)
        .collect();

    let recent_decodes_in_parity: Vec<f64> = self
        .decoded_messages
        .iter()
        .filter(|m| m.timestamp >= cutoff)
        .filter(|m| match (tx_parity, m.slot_parity) {
            (Some(my), Some(theirs)) => my == theirs,
            // tx_parity unknown → treat all decodes as blocking.
            (None, _) => true,
            // decode parity unknown → treat as blocking (safer default).
            (Some(_), None) => true,
        })
        .map(|m| m.delta_freq as f64)
        .collect();

    let mut best: Option<(f64, f64)> = None; // (offset, score)
    let mut hz = MIN_HZ;
    while hz <= MAX_HZ {
        let near_decode = recent_decodes_in_parity
            .iter()
            .any(|&f| (f - hz).abs() <= NEIGHBOR_HZ);
        let near_own = own_freqs
            .iter()
            .any(|&f| (f - hz).abs() <= SEPARATION_HZ);

        if !near_decode && !near_own {
            let spectral = if let Some(row) = latest_row {
                spectral_peak(row, hz, NEIGHBOR_HZ, SPECTRAL_RANGE_HZ)
            } else {
                0.0
            };
            let center_bias = ((hz - 1500.0).abs() / 1300.0) * 0.3;
            let score = spectral as f64 + center_bias;
            best = match best {
                Some((_, prev)) if prev <= score => best,
                _ => Some((hz, score)),
            };
        }
        hz += STEP_HZ;
    }
    let _ = row_len; // silence unused if waterfall_data empty
    best.map(|(hz, _)| hz)
}
```

And add the spectral helper at module level in the same file (above `impl App`):

```rust
/// Peak intensity in the latest waterfall row within ±radius_hz of center_hz.
fn spectral_peak(row: &[f32], center_hz: f64, radius_hz: f64, range: (f64, f64)) -> f32 {
    if row.is_empty() {
        return 0.0;
    }
    let (lo, hi) = range;
    let width = row.len() as f64;
    let bin_hz = (hi - lo) / width;
    if bin_hz <= 0.0 {
        return 0.0;
    }
    let center_bin = ((center_hz - lo) / bin_hz) as isize;
    let radius_bins = (radius_hz / bin_hz).ceil() as isize;
    let lo_bin = center_bin.saturating_sub(radius_bins).max(0) as usize;
    let hi_bin = (center_bin + radius_bins).max(0) as usize;
    let hi_bin = hi_bin.min(row.len() - 1);
    if lo_bin > hi_bin {
        return 0.0;
    }
    row[lo_bin..=hi_bin]
        .iter()
        .copied()
        .fold(0.0f32, f32::max)
}
```

- [ ] **Step 4: Run the tests to confirm they pass**

Run: `cargo test -p pancetta-tui --lib find_clear_offset 2>&1 | tail -20`
Expected: `2 passed; 0 failed`.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "feat(tui): App::find_clear_offset for T-key auto-pick

Scores 25 Hz candidates in 200-2800 Hz against latest waterfall row +
recent decodes in own parity + active QSO frequencies. Returns None
when nothing is clear (operator sees status, cursor stays put).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 9: `T` key + TuiCommand::FindClearOffset

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs`

- [ ] **Step 1: Add the command variant**

In `pancetta-tui/src/tui_runner.rs`, find the `TuiCommand` enum (around line 114). Add the new variant after `StopTx`:

```rust
/// Operator pressed `T` — find a clear TX audio offset and jump the
/// cursor there. TUI-local: the handler calls `App::find_clear_offset`
/// and updates `tx_frequency_offset` directly. No bus message needed.
FindClearOffset,
```

- [ ] **Step 2: Add the key handler**

In the keyboard-handling function (search for `KeyCode::F(8)` and `KeyCode::Char(c)`), add a new arm before the catch-all:

```rust
KeyCode::Char('T') | KeyCode::Char('t') => {
    self.message_tx.send(TuiCommand::FindClearOffset)?;
}
```

- [ ] **Step 3: Handle the command in the runner**

Search for where other `TuiCommand` variants are matched in this file (likely a `match cmd { ... }` block in the runner loop or in a helper). Find the `TuiCommand::StopTx` handler and add alongside it:

```rust
TuiCommand::FindClearOffset => {
    let mut app = self.app.write().await;
    match app.find_clear_offset() {
        Some(hz) => {
            app.tx_frequency_offset = hz;
            app.status_message = format!("TX cursor → {:.0} Hz (clear)", hz);
        }
        None => {
            app.status_message = "No clear offset found in your parity".to_string();
        }
    }
}
```

`tui_runner.rs` has direct `self.app: Arc<RwLock<App>>` access (e.g.
line 220), so handle the command locally without round-tripping
through the coordinator. Other UI-local commands like the `[`/`]`
TX-offset bumps already follow this pattern (see lines 482-491:
they directly mutate `app.tx_frequency_offset` and don't touch
`message_tx`). The `T` key follows the same convention — the only
difference is the command goes through the channel for symmetry
with how F-key commands are wired.

Find the `match` block that handles received `TuiCommand`s (search
for `TuiCommand::ToggleTune =>` or `TuiCommand::StopTx =>`). If no
such block exists, `[`/`]`'s pattern is a hint that some commands
mutate `app` *inside* the key handler directly. In that case, skip
the channel hop entirely for `FindClearOffset`:

```rust
KeyCode::Char('T') | KeyCode::Char('t') => {
    let mut app = self.app.write().await;
    match app.find_clear_offset() {
        Some(hz) => {
            app.tx_frequency_offset = hz;
            app.status_message = format!("TX cursor → {:.0} Hz (clear)", hz);
        }
        None => {
            app.status_message = "No clear offset found in your parity".to_string();
        }
    }
}
```

This keeps the TuiCommand variant for completeness/testing but the
key handler path is local. If the codebase already has a centralized
TuiCommand dispatch loop, prefer that path; otherwise the inline
local mutation matches the surrounding pattern.

- [ ] **Step 4: Build and run TUI tests**

Run: `cargo build -p pancetta-tui 2>&1 | tail -10 && cargo test -p pancetta-tui --lib 2>&1 | tail -15`
Expected: clean build; all tests pass.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): T key — find clear TX offset

Calls App::find_clear_offset and jumps the cursor; status bar reports
the new offset or 'No clear offset found in your parity' if every
candidate is occupied. Lowercase t and uppercase T both fire (no
shift complication on remote keyboards).

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 10: Documentation updates

**Files:**
- Modify: `docs/RUNBOOK.md`, `README.md`

- [ ] **Step 1: Update README key table**

In `README.md`, find the "How to drive the TUI" key table. Add a row:

```markdown
| `T` | **Find clear TX offset** — auto-picks a 25 Hz candidate that's clear in your TX parity, jumps the cursor there. Status bar reports the chosen Hz or "No clear offset found." |
```

If there's also a `F4` Tune row, place `T` near it (both are TX-prep keys).

- [ ] **Step 2: Update RUNBOOK Phase 5 procedure**

In `docs/RUNBOOK.md`, find the Phase 5 procedure for autonomous QSO loop on antenna. Add a bullet near the "Pre-flight checks" or "Choose TX offset" section:

```markdown
- **Pick a clean TX offset:** with the operator's TX parity selected
  (Auto, Even, or Odd in `[station].tx_self_parity`), press `T` to
  auto-pick a 25 Hz candidate. The waterfall's occupancy strip (top
  row) shows the rationale: green/dim = clear, yellow = busy in
  opposite parity (won't collide but courtesy concern), red = busy in
  your parity. The TX cursor (`│`) recolors to match the column it's
  on so manual `[`/`]` adjustments give live feedback.
```

- [ ] **Step 3: Verify no broken doc references**

Run: `grep -rn "F2\|F3\|F4\|F8\|F9\|tx_frequency_offset" docs/RUNBOOK.md README.md | head -10`
Expected: existing references still match the codebase (no stale).

- [ ] **Step 4: Commit**

```bash
git add docs/RUNBOOK.md README.md
git commit -m "docs: T key for waterfall TX-offset auto-pick

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 11: Final integration — full workspace tests + clippy

**Files:**
- None (verification only)

- [ ] **Step 1: Run the full workspace tests**

Per CLAUDE.md, use plain `cargo test` (NOT `--workspace`, which hangs on pancetta-hamlib).

Run:
```bash
cargo test -p pancetta-tui --features transmit 2>&1 | tail -15
cargo test -p pancetta --features transmit 2>&1 | tail -15
cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -15
cargo test -p pancetta-qso --features transmit 2>&1 | tail -15
cargo test -p pancetta-config --features transmit 2>&1 | tail -15
```

Expected: each ends with all tests passing.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --features transmit 2>&1 | tail -30`
Expected: no errors, no new warnings introduced. Address any clippy lints on lines we touched in this plan.

- [ ] **Step 3: Run cargo fmt**

Run: `cargo fmt --all -- --check`
Expected: no diff. If formatted differently, run `cargo fmt --all`, stage the result, and amend the most recent commit (pre-push hook will reject otherwise).

- [ ] **Step 4: Push to main**

```bash
git push
```

Expected: pre-push hook (`scripts/check.sh`) runs fmt+clippy and passes; push succeeds.

---

## Notes for the implementer

- **Don't re-introduce per-row min/max anywhere.** Task 2 deliberately replaces it; if you see it creep back during Task 5/6 builds, check the diff.
- **Test data realism:** the widget tests use `Vec<Vec<f32>>` of any consistent shape — width 64 is fine for unit tests, the live data is ~1024 bins. The tests verify behavior at the column level, not bin counts.
- **The `T` key handler** lives in the TUI runner if `self.app` is reachable there. If it's not, the fallback is to wire through `tui_relay.rs` — note the migration in the Task 9 commit message so the next maintainer can find it.
- **`tx_self_parity` config field** — Task 3 assumes `pancetta_config::station::TxSelfParity::{Auto, Even, Odd}` exists (per CLAUDE.md it does, default Auto). If the variants differ, adjust the match arms in `resolve_tx_parity` accordingly.
- **`fixture_view` helper** — Task 8's tests reference `fixture_view(call, snr)` already in the test module (`pancetta-tui/src/app.rs:1156`). The new `fixture_view_at` builds on top.
- **Memory-noted policies followed by this plan:**
  - Each task ends with a commit. (`feedback_commit_after_changes`)
  - Doc updates are in Task 10. (`feedback_doc_updates`)
  - Tests run autonomously per task; no operator handoff. (`feedback_autonomous_testing`)
  - Plain `cargo test` (no `--workspace`). (`feedback_workspace_test_hamlib`)
  - Pre-push hook is honored — Task 11 runs fmt+clippy before push. (no `--no-verify`)
