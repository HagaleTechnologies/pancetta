# Waterfall SSH Rendering & Pipeline Diagnostics

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix waterfall rendering over SSH (256-color fallback) and add pipeline health diagnostics so operators can see at a glance whether audio/DSP/decoder are alive.

**Architecture:** Detect terminal color capability at TUI startup via `TERM`/`COLORTERM` env vars and auto-select a compatible waterfall color scheme. Add pipeline health state to `App` and surface it in the status bar. Also fix a bug where waterfall signal markers use operating frequency (MHz) instead of audio offset (Hz).

**Tech Stack:** Rust, ratatui, crossterm, std::env

---

### Task 1: Add terminal color detection and waterfall fallback

**Files:**
- Modify: `pancetta-tui/src/widgets/mod.rs:81-117` (color scheme logic)
- Modify: `pancetta-tui/src/ui/mod.rs:261-284` (render_waterfall)
- Modify: `pancetta-tui/src/app.rs:257,323` (store detected color capability)

- [ ] **Step 1: Add color capability detection to App**

In `pancetta-tui/src/app.rs`, add a `ColorCapability` enum and detect it at startup:

```rust
/// Terminal color support level, detected at startup
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorCapability {
    /// 256-color (xterm-256color, COLORTERM=256color, etc.)
    TwoFiftySix,
    /// Basic 16-color (most terminals, including SSH defaults)
    Basic,
}

impl ColorCapability {
    pub fn detect() -> Self {
        // COLORTERM=truecolor or 24bit implies 256-color support too
        if let Ok(ct) = std::env::var("COLORTERM") {
            let ct = ct.to_lowercase();
            if ct == "truecolor" || ct == "24bit" || ct == "256color" {
                return Self::TwoFiftySix;
            }
        }
        // Check TERM for 256color suffix
        if let Ok(term) = std::env::var("TERM") {
            if term.contains("256color") {
                return Self::TwoFiftySix;
            }
        }
        Self::Basic
    }
}
```

Add `pub color_capability: ColorCapability` field to the `App` struct (after `audio_level`), and initialize it with `ColorCapability::detect()` in `App::new()`.

- [ ] **Step 2: Add a basic-color waterfall scheme to the Waterfall widget**

In `pancetta-tui/src/widgets/mod.rs`, update `get_color_for_intensity` to accept a `ColorCapability` parameter and add a basic-color gradient for `Basic` terminals:

```rust
fn get_color_for_intensity(&self, intensity: f32, color_cap: ColorCapability) -> Color {
    let clamped = intensity.clamp(0.0, 1.0);

    // Force basic colors if terminal doesn't support 256-color
    if color_cap == ColorCapability::Basic {
        // 7-step gradient using basic 16 colors — works everywhere
        return if clamped < 0.15 {
            Color::Black
        } else if clamped < 0.30 {
            Color::DarkGray
        } else if clamped < 0.45 {
            Color::Blue
        } else if clamped < 0.60 {
            Color::Cyan
        } else if clamped < 0.75 {
            Color::Gray
        } else if clamped < 0.90 {
            Color::White
        } else {
            Color::Yellow
        };
    }

    match self.color_scheme {
        // ... existing Classic/Spectrum/Thermal logic unchanged
    }
}
```

Add `color_capability: ColorCapability` field to the `Waterfall` struct, defaulting to `TwoFiftySix`. Add a builder method:

```rust
pub fn color_capability(mut self, cap: ColorCapability) -> Self {
    self.color_capability = cap;
    self
}
```

Update the `render` method to pass `self.color_capability` to `get_color_for_intensity`.

- [ ] **Step 3: Wire color capability through render_waterfall**

In `pancetta-tui/src/ui/mod.rs`, update `render_waterfall` to pass the app's color capability:

```rust
let waterfall = Waterfall::new(&app.waterfall_data)
    .block(waterfall_block)
    .tx_offset(app.tx_frequency_offset)
    .signal_freqs(signal_freqs)
    .color_capability(app.color_capability);
```

- [ ] **Step 4: Log detected capability at startup**

In `pancetta-tui/src/app.rs`, in `App::new()`, after detecting color capability:

```rust
tracing::info!("Terminal color capability: {:?}", color_capability);
```

- [ ] **Step 5: Build and verify**

Run: `cargo build -p pancetta-tui`
Expected: Compiles without errors.

- [ ] **Step 6: Commit**

```bash
git add pancetta-tui/src/widgets/mod.rs pancetta-tui/src/app.rs pancetta-tui/src/ui/mod.rs
git commit -m "fix: waterfall auto-detect terminal color support, fallback for SSH"
```

---

### Task 2: Fix waterfall signal markers using wrong frequency field

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs:264-269`

The waterfall signal markers use `m.frequency` (operating frequency in MHz, e.g. 14.074) but the waterfall X-axis is audio offset in Hz (0-3000). They should use `m.delta_freq` (audio frequency offset).

- [ ] **Step 1: Fix signal_freqs mapping**

In `pancetta-tui/src/ui/mod.rs`, change the signal_freqs collection:

```rust
let signal_freqs: Vec<f64> = app
    .decoded_messages
    .iter()
    .filter(|m| m.timestamp > cutoff)
    .map(|m| m.delta_freq as f64)
    .collect();
```

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-tui`
Expected: Compiles without errors.

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/ui/mod.rs
git commit -m "fix: waterfall signal markers use audio offset, not operating freq"
```

---

### Task 3: Add ft8_lib availability detection function

**Files:**
- Modify: `pancetta-ft8/src/ft8_lib_ffi.rs` (add `is_available()`)
- Modify: `pancetta-ft8/src/lib.rs` (re-export)

- [ ] **Step 1: Add `ft8lib_is_available` function**

In `pancetta-ft8/src/ft8_lib_ffi.rs`, add at the bottom:

```rust
/// Returns `true` when the real ft8_lib C library is compiled in,
/// `false` when using the pure-Rust stub fallback.
pub fn ft8lib_is_available() -> bool {
    cfg!(not(ft8lib_stub))
}
```

- [ ] **Step 2: Re-export from lib.rs**

In `pancetta-ft8/src/lib.rs`, add to the public API:

```rust
pub use ft8_lib_ffi::ft8lib_is_available;
```

- [ ] **Step 3: Build and verify**

Run: `cargo build -p pancetta-ft8`
Expected: Compiles without errors.

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/src/ft8_lib_ffi.rs pancetta-ft8/src/lib.rs
git commit -m "feat: expose ft8lib_is_available() for pipeline diagnostics"
```

---

### Task 4: Add pipeline health tracking to TUI

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs:76-101` (add TuiMessage variant)
- Modify: `pancetta-tui/src/app.rs` (add health state)
- Modify: `pancetta-tui/src/tui_runner.rs:~302` (handle new message)

- [ ] **Step 1: Add PipelineHealth struct and TuiMessage variant**

In `pancetta-tui/src/app.rs`, add after the `DecodedMessageView` struct:

```rust
/// Pipeline component health snapshot, forwarded from coordinator
#[derive(Debug, Clone)]
pub struct PipelineHealth {
    /// Audio thread alive and producing samples
    pub audio_alive: bool,
    /// Number of DSP windows sent to decoder
    pub dsp_windows: u64,
    /// Audio RMS of last DSP window (0.0 = silence)
    pub last_rms: f32,
    /// Whether ft8_lib C decoder is compiled (vs stub)
    pub ft8lib_available: bool,
    /// Total messages decoded this session
    pub total_decodes: u64,
}
```

Add `pub pipeline_health: Option<PipelineHealth>` to the `App` struct, initialize as `None`.

In `pancetta-tui/src/tui_runner.rs`, add a new TuiMessage variant:

```rust
/// Pipeline health snapshot (sent periodically by coordinator)
PipelineHealth(crate::app::PipelineHealth),
```

Handle it in the message processing match:

```rust
TuiMessage::PipelineHealth(health) => {
    app.pipeline_health = Some(health);
}
```

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-tui`
Expected: Compiles without errors.

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs
git commit -m "feat: add PipelineHealth struct and TuiMessage variant"
```

---

### Task 5: Display pipeline health in the TUI status bar

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs:167-258` (render_status_bar)

- [ ] **Step 1: Add health indicators to status bar**

In `pancetta-tui/src/ui/mod.rs`, update `render_status_bar` to show pipeline health. Replace the `audio_status` logic with:

```rust
// Pipeline health indicators
let (audio_indicator, dsp_indicator, decoder_indicator) = match &app.pipeline_health {
    Some(health) => {
        let audio = if health.audio_alive {
            Span::styled("AUD", Style::default().fg(app.theme.success_color()).add_modifier(Modifier::BOLD))
        } else {
            Span::styled("AUD", Style::default().fg(app.theme.error_color()).add_modifier(Modifier::BOLD))
        };
        let dsp = if health.dsp_windows > 0 {
            Span::styled(
                format!("DSP:{}", health.dsp_windows),
                Style::default().fg(app.theme.success_color()),
            )
        } else {
            Span::styled("DSP:0", Style::default().fg(app.theme.error_color()))
        };
        let dec_label = if health.ft8lib_available { "FT8" } else { "FT8(native)" };
        let decoder = if health.total_decodes > 0 {
            Span::styled(
                format!("{}:{}", dec_label, health.total_decodes),
                Style::default().fg(app.theme.success_color()),
            )
        } else {
            Span::styled(
                format!("{}:0", dec_label),
                Style::default().fg(app.theme.warning_color()),
            )
        };
        (audio, dsp, decoder)
    }
    None => (
        Span::styled("AUD", Style::default().fg(app.theme.muted_color())),
        Span::styled("DSP", Style::default().fg(app.theme.muted_color())),
        Span::styled("FT8", Style::default().fg(app.theme.muted_color())),
    ),
};
```

Then update the `status_line` to include these:

```rust
let status_line = Line::from(vec![
    audio_indicator,
    Span::raw(" "),
    dsp_indicator,
    Span::raw(" "),
    decoder_indicator,
    Span::raw(" | "),
    Span::styled(
        format!("Level: {:.1}%", app.audio_level * 100.0),
        Style::default().fg(app.theme.foreground_color()),
    ),
    Span::raw(" | "),
    Span::styled(
        format!("Msgs: {}", messages_count),
        Style::default().fg(app.theme.foreground_color()),
    ),
    Span::raw(" | "),
    Span::styled(
        format!("DX: {}", dx_count),
        Style::default().fg(app.theme.foreground_color()),
    ),
    Span::raw(" | "),
    Span::styled(
        &app.status_message,
        Style::default().fg(app.theme.accent_color()),
    ),
]);
```

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-tui`
Expected: Compiles without errors.

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/ui/mod.rs
git commit -m "feat: pipeline health indicators in TUI status bar"
```

---

### Task 6: Send pipeline health from coordinator to TUI

**Files:**
- Modify: `pancetta/src/coordinator/pipeline.rs` (health tracking + periodic send)

- [ ] **Step 1: Add health tracking state and periodic health sender in the TUI relay thread**

In `pancetta/src/coordinator/pipeline.rs`, in the `start_ft8_pipeline` function, add counters that track window count and decode count. These already exist partially (`window_count` in DSP, decode counts in FT8). The coordinator needs to aggregate and send to TUI.

Add shared atomics before `start_pipeline()` is called. In the `start_pipeline` function, after creating channels:

```rust
// Pipeline health tracking (atomics shared across threads)
let health_dsp_windows = Arc::new(std::sync::atomic::AtomicU64::new(0));
let health_total_decodes = Arc::new(std::sync::atomic::AtomicU64::new(0));
let health_last_rms = Arc::new(std::sync::atomic::AtomicU32::new(0)); // f32 bits
let health_audio_alive = Arc::new(std::sync::atomic::AtomicBool::new(false));
```

Pass clones to the DSP thread (increment `health_dsp_windows` and `health_last_rms` when sending a window) and FT8 thread (increment `health_total_decodes` per decoded message).

In the TUI relay thread, add a periodic health sender (every 2 seconds):

```rust
let mut last_health_send = std::time::Instant::now();
// ... inside the relay loop, after existing try_recv blocks:
if last_health_send.elapsed() >= std::time::Duration::from_secs(2) {
    let health = pancetta_tui::app::PipelineHealth {
        audio_alive: health_audio_alive.load(Ordering::Relaxed),
        dsp_windows: health_dsp_windows.load(Ordering::Relaxed),
        last_rms: f32::from_bits(health_last_rms.load(Ordering::Relaxed)),
        ft8lib_available: pancetta_ft8::ft8lib_is_available(),
        total_decodes: health_total_decodes.load(Ordering::Relaxed),
    };
    let _ = tui_msg_tx_relay.send(
        pancetta_tui::tui_runner::TuiMessage::PipelineHealth(health),
    );
    last_health_send = std::time::Instant::now();
}
```

- [ ] **Step 2: Increment counters in DSP thread**

In the DSP `spawn_blocking` closure, after `dsp_to_ft8_tx.send(window)` succeeds (around line 682), add:

```rust
health_dsp_windows.fetch_add(1, Ordering::Relaxed);
health_last_rms.store(rms.to_bits(), Ordering::Relaxed);
```

- [ ] **Step 3: Increment counters in FT8 decoder thread**

In the FT8 `spawn_blocking` closure, after `decoded_messages.len()` is known (around line 845), add:

```rust
health_total_decodes.fetch_add(decoded_messages.len() as u64, Ordering::Relaxed);
```

- [ ] **Step 4: Set audio_alive flag in audio relay**

In the audio relay task (the async relay from tokio mpsc to crossbeam, around line 399), after the first successful `audio_to_dsp_tx.send()`, set:

```rust
health_audio_alive.store(true, Ordering::Relaxed);
```

- [ ] **Step 5: Add startup diagnostic log**

At the top of `start_pipeline()`, after creating all channels, log:

```rust
info!(
    "Pipeline starting: ft8_lib={}, audio_device={}",
    if pancetta_ft8::ft8lib_is_available() { "native-C" } else { "stub (pure-Rust only)" },
    if self.headless { "stub" } else { "real" },
);
```

- [ ] **Step 6: Build and verify full workspace**

Run: `cargo build`
Expected: Compiles without errors.

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/coordinator/pipeline.rs
git commit -m "feat: pipeline health tracking — audio/DSP/decoder status to TUI"
```

---

### Task 7: Add headless health drain

**Files:**
- Modify: `pancetta/src/coordinator/pipeline.rs:216-250` (headless drain)

- [ ] **Step 1: Log health periodically in headless mode**

In the headless drain task (around line 216), the existing code drains `ft8_to_tui_rx` and `waterfall_rx`. There's no TUI to show health, so log a periodic summary. Add a counter and log every 4 windows (~60 seconds):

```rust
// Inside the headless drain loop, add:
if window_drain_count % 4 == 0 && window_drain_count > 0 {
    info!(
        "Pipeline health: ft8_lib={}, dsp_windows={}, total_decodes={}, audio={}",
        if pancetta_ft8::ft8lib_is_available() { "C" } else { "stub" },
        health_dsp_windows.load(Ordering::Relaxed),
        health_total_decodes.load(Ordering::Relaxed),
        if health_audio_alive.load(Ordering::Relaxed) { "alive" } else { "no-data" },
    );
}
```

- [ ] **Step 2: Build and verify**

Run: `cargo build`
Expected: Compiles without errors.

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/coordinator/pipeline.rs
git commit -m "feat: periodic pipeline health logging in headless mode"
```

---

### Task 8: Re-export PipelineHealth from pancetta-tui lib.rs

**Files:**
- Modify: `pancetta-tui/src/lib.rs:36`

- [ ] **Step 1: Add PipelineHealth to re-exports**

```rust
pub use app::{
    ActivePanel, App, AutonomousStatus, DecodedMessageView, DevicePanel, DeviceSelectionState,
    DxStation, PipelineHealth, QsoStatus, StationInfo,
};
```

- [ ] **Step 2: Build and verify**

Run: `cargo build`
Expected: Compiles without errors.

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/lib.rs
git commit -m "chore: re-export PipelineHealth from pancetta-tui"
```

---

### Task 9: Run loopback test to verify no regressions

**Files:** None (verification only)

- [ ] **Step 1: Run workspace tests**

Run: `cargo test`
Expected: All existing tests pass.

- [ ] **Step 2: Run FT8 tests with transmit feature**

Run: `cargo test --features transmit -p pancetta-ft8`
Expected: All ~295 tests pass.

- [ ] **Step 3: Run loopback integration test**

Run: `cargo test -p pancetta --test loopback_qso`
Expected: Loopback QSO test passes.

- [ ] **Step 4: Commit any fixups if needed, then final push**

```bash
git push
```
