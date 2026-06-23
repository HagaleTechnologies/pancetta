# Unwrap Hardening + TUI Pipeline Completion — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Eliminate dangerous `unwrap()` calls in production code and complete the TUI pipeline for receive-only operation (real frequency, distance/bearing, audio monitoring clarity).

**Architecture:** Two independent workstreams touching different files. Workstream 1 (Tasks 1-5) hardens error handling. Workstream 2 (Tasks 6-8) completes TUI data enrichment. Each task is independently committable.

**Tech Stack:** Rust, chrono, crossbeam-channel, pancetta-dx geography/gridsquare modules, thiserror

---

## Workstream 1: Unwrap Hardening

### Task 1: Fix signal handler expects in main.rs

**Files:**
- Modify: `pancetta/src/main.rs:278-298`

- [ ] **Step 1: Replace signal handler expect with error logging**

The two `expect()` calls are inside `tokio::spawn` blocks, which return `JoinHandle<()>` — they can't propagate `Result`. The correct fix is to log the error and trigger shutdown rather than panicking.

Edit `pancetta/src/main.rs` — replace lines 278-291:

```rust
    tokio::spawn(async move {
        match Signals::new(&[SIGINT]) {
            Ok(mut signals) => {
                while let Some(signal) = signals.next().await {
                    match signal {
                        SIGINT => {
                            info!("Received SIGINT, initiating graceful shutdown");
                            shutdown_clone.store(true, Ordering::Release);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!("Failed to register signal handler: {}", e);
                shutdown_clone.store(true, Ordering::Release);
            }
        }
    });
```

Then replace lines 294-298:

```rust
    tokio::spawn(async move {
        if let Err(e) = signal::ctrl_c().await {
            error!("Failed to listen for ctrl+c: {}", e);
        }
        warn!("Received Ctrl+C, initiating graceful shutdown");
        shutdown_for_signals.store(true, Ordering::Release);
    });
```

- [ ] **Step 2: Verify the `error!` macro is imported**

Check that `tracing::error` is in the imports at the top of main.rs. If not, add it. The file already imports from tracing — check the exact import line and add `error` if missing.

- [ ] **Step 3: Run tests**

Run: `cargo check -p pancetta`
Expected: Compiles with no errors

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/main.rs
git commit -m "fix: replace signal handler panics with graceful error logging

Signal handler setup failures now log the error and trigger shutdown
instead of panicking the application.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 2: Fix date/time unwraps in statistics.rs

**Files:**
- Modify: `pancetta-qso/src/statistics.rs:14-29,1021-1040,1166-1170`

- [ ] **Step 1: Add InvalidDate variant to StatisticsError**

Edit `pancetta-qso/src/statistics.rs` — after the existing `InsufficientData` variant (line 28), add:

```rust
    #[error("Invalid date: year {year}")]
    InvalidDate { year: i32 },
```

- [ ] **Step 2: Create a helper function for safe date construction**

Add this private helper near the top of the `impl` block (after the struct definition, before `calculate_statistics`). This avoids repeating the same `.ok_or_else()` chain 8 times:

```rust
    /// Safely construct a UTC DateTime from year/month/day/hour/min/sec.
    fn make_utc(
        year: i32,
        month: u32,
        day: u32,
        hour: u32,
        min: u32,
        sec: u32,
    ) -> Result<DateTime<Utc>, StatisticsError> {
        chrono::NaiveDate::from_ymd_opt(year, month, day)
            .and_then(|d| d.and_hms_opt(hour, min, sec))
            .map(|dt| dt.and_utc())
            .ok_or(StatisticsError::InvalidDate { year })
    }
```

- [ ] **Step 3: Replace unwraps in calculate_yearly_comparison**

Edit `pancetta-qso/src/statistics.rs` — replace the 4 date constructions in `calculate_yearly_comparison` (lines 1021-1040). Replace:

```rust
        let start1 = chrono::NaiveDate::from_ymd_opt(year1 as i32, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end1 = chrono::NaiveDate::from_ymd_opt(year1 as i32, 12, 31)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_utc();

        let start2 = chrono::NaiveDate::from_ymd_opt(year2 as i32, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc();
        let end2 = chrono::NaiveDate::from_ymd_opt(year2 as i32, 12, 31)
            .unwrap()
            .and_hms_opt(23, 59, 59)
            .unwrap()
            .and_utc();
```

With:

```rust
        let start1 = Self::make_utc(year1 as i32, 1, 1, 0, 0, 0)?;
        let end1 = Self::make_utc(year1 as i32, 12, 31, 23, 59, 59)?;
        let start2 = Self::make_utc(year2 as i32, 1, 1, 0, 0, 0)?;
        let end2 = Self::make_utc(year2 as i32, 12, 31, 23, 59, 59)?;
```

- [ ] **Step 4: Fix the and_hms_opt unwrap at line 1170**

Replace:

```rust
            .map(|(date, _)| chrono::Utc.from_utc_datetime(&date.and_hms_opt(0, 0, 0).unwrap()));
```

With:

```rust
            .and_then(|(date, _)| {
                date.and_hms_opt(0, 0, 0)
                    .map(|dt| chrono::Utc.from_utc_datetime(&dt))
            });
```

This changes the `Option<DateTime>` to `None` if the time construction fails, which is the correct semantic — the max_day becomes `None` rather than panicking.

- [ ] **Step 5: Fix first()/last() unwraps in session calculation (lines 1604-1625)**

These are guarded by `if !current_session.is_empty()` checks. Change `.unwrap()` to `.expect("checked non-empty")` for documentation. Replace all 6 occurrences in the two blocks:

Lines 1607-1611 — replace:
```rust
                        let session_start = current_session.first().unwrap().metadata.start_time;
                        let session_end =
                            current_session.last().unwrap().metadata.end_time.unwrap_or(
                                current_session.last().unwrap().metadata.start_time
                                    + Duration::minutes(2),
                            );
```

With:
```rust
                        let session_start = current_session.first().expect("checked non-empty").metadata.start_time;
                        let session_end =
                            current_session.last().expect("checked non-empty").metadata.end_time.unwrap_or(
                                current_session.last().expect("checked non-empty").metadata.start_time
                                    + Duration::minutes(2),
                            );
```

Lines 1622-1624 — apply the same `.expect("checked non-empty")` replacement to the second block (identical pattern).

- [ ] **Step 6: Run tests**

Run: `cargo test -p pancetta-qso`
Expected: All tests pass

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/statistics.rs
git commit -m "fix: replace dangerous unwraps with proper error handling in statistics

Add StatisticsError::InvalidDate variant. Replace date/time construction
unwraps with a make_utc() helper that propagates errors. Document
first()/last() unwraps that are guarded by emptiness checks.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 3: Fix unwraps in autonomous.rs

**Files:**
- Modify: `pancetta-qso/src/autonomous.rs:183-188,639`

- [ ] **Step 1: Fix our_slot.unwrap() at line 185**

This unwrap is actually safe — `our_slot` is set to `Some(...)` on line 176, two lines above. But the `unwrap()` in a format string is fragile. Replace:

```rust
            info!(
                "Auto-detected TX parity: {:?} (even={}, odd={})",
                self.our_slot.unwrap(),
                self.auto_detect_even_activity,
                self.auto_detect_odd_activity,
            );
```

With:

```rust
            info!(
                "Auto-detected TX parity: {:?} (even={}, odd={})",
                self.our_slot.expect("just assigned above"),
                self.auto_detect_even_activity,
                self.auto_detect_odd_activity,
            );
```

- [ ] **Step 2: Fix partial_cmp unwrap at line 639**

Replace:

```rust
                .min_by(|a, b| a.partial_cmp(b).unwrap())
```

With:

```rust
                .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
```

- [ ] **Step 3: Run tests**

Run: `cargo test -p pancetta-qso`
Expected: All tests pass

- [ ] **Step 4: Commit**

```bash
git add pancetta-qso/src/autonomous.rs
git commit -m "fix: handle NaN in frequency comparison, document slot unwrap

Use unwrap_or(Equal) for f64 partial_cmp to handle NaN gracefully.
Document the our_slot expect with reasoning.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 4: Fix capture group unwraps in exchange.rs

**Files:**
- Modify: `pancetta-qso/src/exchange.rs:12-31,395,442-443`

- [ ] **Step 1: Add MissingCapture variant to ExchangeError**

Edit `pancetta-qso/src/exchange.rs` — after the existing `ParseError` variant (line 30), add:

```rust
    #[error("Missing capture group {group} in message: {message}")]
    MissingCapture { group: usize, message: String },
```

- [ ] **Step 2: Fix captures.get(3).unwrap() at line 395**

Replace:

```rust
            let third_field = captures.get(3).unwrap().as_str();
```

With:

```rust
            let third_field = captures
                .get(3)
                .ok_or_else(|| ExchangeError::MissingCapture {
                    group: 3,
                    message: message.to_string(),
                })?
                .as_str();
```

- [ ] **Step 3: Fix captures.get(3) and get(4) unwraps at lines 442-443**

Replace:

```rust
            let report_str = captures.get(3).unwrap().as_str();
            let serial_str = captures.get(4).unwrap().as_str();
```

With:

```rust
            let report_str = captures
                .get(3)
                .ok_or_else(|| ExchangeError::MissingCapture {
                    group: 3,
                    message: message.to_string(),
                })?
                .as_str();
            let serial_str = captures
                .get(4)
                .ok_or_else(|| ExchangeError::MissingCapture {
                    group: 4,
                    message: message.to_string(),
                })?
                .as_str();
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p pancetta-qso`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/exchange.rs
git commit -m "fix: replace regex capture unwraps with proper error propagation

Add ExchangeError::MissingCapture variant. Capture group access now
returns descriptive errors instead of panicking on malformed input.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 5: Fix date/time unwraps in geography.rs

**Files:**
- Modify: `pancetta-dx/src/geography.rs:460-540`

- [ ] **Step 1: Add a safe time construction helper**

The geography module uses `crate::Result` which wraps `DxError`. Add a helper at the top of the `calculate_sunrise_sunset` function body (or as a module-level private function):

```rust
/// Safely create a NaiveDateTime, returning a fallback on invalid h/m/s.
fn safe_hms(date: chrono::NaiveDate, h: u32, m: u32, s: u32, fallback_h: u32) -> chrono::NaiveDateTime {
    date.and_hms_opt(h, m, s)
        .unwrap_or_else(|| date.and_hms_opt(fallback_h, 0, 0)
            .unwrap_or_else(|| date.and_hms_opt(0, 0, 0)
                .expect("midnight is always valid")))
}
```

Note: `and_hms_opt(0, 0, 0)` on a valid `NaiveDate` is infallible (h=0, m=0, s=0 are always valid), so the innermost `expect` is genuinely safe.

- [ ] **Step 2: Replace polar condition unwraps (lines 466, 470)**

Replace:

```rust
        let midnight = date.and_hms_opt(0, 0, 0).unwrap().and_utc();
        return Ok((midnight, midnight));
    } else if cos_hour_angle < -1.0 {
        let noon = date.and_hms_opt(12, 0, 0).unwrap().and_utc();
```

With:

```rust
        let midnight = safe_hms(date, 0, 0, 0, 0).and_utc();
        return Ok((midnight, midnight));
    } else if cos_hour_angle < -1.0 {
        let noon = safe_hms(date, 12, 0, 0, 12).and_utc();
```

- [ ] **Step 3: Replace sunrise/sunset fallback unwraps (lines 491-492, 509-510, 518-519, 536-537)**

Replace all 4 `unwrap_or_else` blocks that contain inner unwraps. For sunrise (lines 491-492):

Replace:
```rust
        date.and_hms_opt(hour, minute, second)
            .unwrap_or_else(|| date.and_hms_opt(12, 0, 0).unwrap())
            .and_utc()
```

With:
```rust
        safe_hms(date, hour, minute, second, 12).and_utc()
```

For sunrise day-boundary (lines 509-510):
Replace:
```rust
        adjusted_date
            .and_hms_opt(hour, minute, second)
            .unwrap_or_else(|| date.and_hms_opt(6, 0, 0).unwrap())
            .and_utc()
```

With:
```rust
        safe_hms(adjusted_date, hour, minute, second, 6).and_utc()
```

For sunset (lines 518-519):
Replace:
```rust
        date.and_hms_opt(hour, minute, second)
            .unwrap_or_else(|| date.and_hms_opt(18, 0, 0).unwrap())
            .and_utc()
```

With:
```rust
        safe_hms(date, hour, minute, second, 18).and_utc()
```

For sunset day-boundary (lines 536-537):
Replace:
```rust
        adjusted_date
            .and_hms_opt(hour, minute, second)
            .unwrap_or_else(|| date.and_hms_opt(18, 0, 0).unwrap())
            .and_utc()
```

With:
```rust
        safe_hms(adjusted_date, hour, minute, second, 18).and_utc()
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p pancetta-dx`
Expected: All tests pass

- [ ] **Step 5: Commit**

```bash
git add pancetta-dx/src/geography.rs
git commit -m "fix: eliminate unwrap chains in sunrise/sunset calculation

Replace nested unwrap_or_else(|| .unwrap()) patterns with a safe_hms()
helper that cascades to valid fallback times. Midnight construction
(0,0,0) is genuinely infallible on a valid NaiveDate.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Workstream 2: TUI Pipeline Completion

### Task 6: Fix hardcoded operating frequency in coordinator relay

**Files:**
- Modify: `pancetta/src/coordinator.rs:869-963`

- [ ] **Step 1: Capture operating frequency in relay closure**

Before the relay task spawn (line 876), read the operating frequency from config and create a shared mutable reference for the relay:

Add before `let relay_handle = tokio::spawn(async move {`:

```rust
        // Read initial operating frequency from config
        let operating_freq_mhz = {
            let config = self.config.read().unwrap_or_else(|e| e.into_inner());
            config.station.frequency_mhz.unwrap_or(14.074)
        };
        let operating_freq = Arc::new(std::sync::atomic::AtomicU64::new(
            operating_freq_mhz.to_bits(),
        ));
        let operating_freq_relay = operating_freq.clone();
```

Note: We store the f64 as AtomicU64 bits to allow atomic updates from the FrequencyResponse handler.

- [ ] **Step 2: Update frequency from hamlib messages in the relay loop**

Inside the relay loop, where `FrequencyResponse` is handled (lines 920-931), add a frequency update. After sending the TuiMessage, also update the atomic:

```rust
                            MessageType::RigControl(
                                crate::message_bus::RigControlMessage::FrequencyResponse {
                                    vfo,
                                    frequency,
                                },
                            ) => {
                                // Update operating frequency for decoded message enrichment
                                let freq_mhz = frequency as f64 / 1_000_000.0;
                                operating_freq_relay.store(freq_mhz.to_bits(), Ordering::Relaxed);

                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::FrequencyUpdate {
                                        vfo,
                                        frequency,
                                    },
                                );
                            }
```

- [ ] **Step 3: Use operating frequency in DecodedMessageView construction**

Replace the hardcoded frequency (line 885-886) in the relay. Change:

```rust
                        let tui_decoded = pancetta_tui::DecodedMessageView {
                            timestamp: chrono::Utc::now(),
                            frequency: 14.074,
```

To:

```rust
                        let current_freq = f64::from_bits(
                            operating_freq_relay.load(Ordering::Relaxed),
                        );
                        let tui_decoded = pancetta_tui::DecodedMessageView {
                            timestamp: chrono::Utc::now(),
                            frequency: current_freq,
```

- [ ] **Step 4: Check that station config has frequency_mhz field**

Read `pancetta-config/src/station.rs` to verify the field name. If it doesn't exist as `frequency_mhz`, use the default band frequency or add the field. The station config struct may use a different field name — adapt accordingly.

If no frequency field exists in station config, use `14.074` as default and add a `// TODO: add frequency_mhz to StationConfig` comment.

- [ ] **Step 5: Run tests**

Run: `cargo check -p pancetta`
Expected: Compiles with no errors

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "fix: use actual operating frequency in TUI relay instead of hardcoded 14.074

Store operating frequency as AtomicU64 (f64 bits) in the relay task.
Initialize from config, update on hamlib FrequencyResponse messages.
Decoded messages now show the real operating frequency.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 7: Compute distance and bearing from grid squares

**Files:**
- Modify: `pancetta/src/coordinator.rs:869-900`

- [ ] **Step 1: Capture station grid and create geography calculator**

Before the relay task spawn, read the station grid from config and set up the calculator. Add alongside the operating frequency setup from Task 6:

```rust
        // Set up geography calculator for distance/bearing from station grid
        let station_grid = {
            let config = self.config.read().unwrap_or_else(|e| e.into_inner());
            config.station.grid_square.clone()
        };
        let station_coords = pancetta_dx::gridsquare::grid_to_coordinates(&station_grid).ok();
```

These values are captured by the relay closure (they're `String` and `Option<(f64, f64)>`, both `Send`).

- [ ] **Step 2: Compute distance/bearing in the relay when grid is available**

In the relay task, after constructing `call_sign` and `grid_square` from the decoded message (lines 880-881), compute distance and bearing. Replace the `DecodedMessageView` construction:

Replace:
```rust
                        let tui_decoded = pancetta_tui::DecodedMessageView {
                            timestamp: chrono::Utc::now(),
                            frequency: current_freq,
                            mode: "FT8".to_string(),
                            snr: decoded_msg.snr_db as i32,
                            delta_time: decoded_msg.time_offset as f32,
                            delta_freq: decoded_msg.frequency_offset as f32,
                            call_sign,
                            grid_square,
                            message: decoded_msg.text.clone(),
                            distance: None,
                            bearing: None,
                        };
```

With:
```rust
                        // Compute distance and bearing if both grids are available
                        let (distance, bearing) = match (&grid_square, &station_coords) {
                            (Some(remote_grid), Some((home_lat, home_lon))) => {
                                match pancetta_dx::gridsquare::grid_to_coordinates(remote_grid) {
                                    Ok((remote_lat, remote_lon)) => {
                                        let geod = geographiclib_rs::Geodesic::wgs84();
                                        let (dist_m, azi1, _azi2) =
                                            geod.inverse(*home_lat, *home_lon, remote_lat, remote_lon);
                                        let bearing_deg = if azi1 < 0.0 { azi1 + 360.0 } else { azi1 };
                                        (Some(dist_m / 1000.0), Some(bearing_deg))
                                    }
                                    Err(_) => (None, None),
                                }
                            }
                            _ => (None, None),
                        };

                        let tui_decoded = pancetta_tui::DecodedMessageView {
                            timestamp: chrono::Utc::now(),
                            frequency: current_freq,
                            mode: "FT8".to_string(),
                            snr: decoded_msg.snr_db as i32,
                            delta_time: decoded_msg.time_offset as f32,
                            delta_freq: decoded_msg.frequency_offset as f32,
                            call_sign,
                            grid_square,
                            message: decoded_msg.text.clone(),
                            distance,
                            bearing,
                        };
```

- [ ] **Step 3: Add geographiclib-rs to pancetta crate dependencies**

Check `pancetta/Cargo.toml` — if `geographiclib-rs` is not listed, add it. It's already a workspace dependency. Add under `[dependencies]`:

```toml
geographiclib-rs.workspace = true
```

Also ensure `pancetta-dx` is a dependency (it already is at line 29).

- [ ] **Step 4: Run tests**

Run: `cargo check -p pancetta`
Expected: Compiles with no errors

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator.rs pancetta/Cargo.toml
git commit -m "feat: compute distance and bearing from grid squares in TUI relay

When a decoded FT8 message includes a grid square and the station grid
is configured, compute great-circle distance (km) and bearing (degrees)
using WGS84 geodesic. Results populate the DX hunter and band activity
panels.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

### Task 8: Clarify audio monitoring stub

**Files:**
- Modify: `pancetta-tui/src/app.rs:427-433`

- [ ] **Step 1: Replace the TODO stub with documentation**

The audio monitoring stub in the TUI is correct behavior — the coordinator is responsible for pipeline setup. The TUI just needs to know data is flowing. Replace:

```rust
    async fn start_audio_monitoring(&mut self, _device: &str) -> Result<()> {
        // TODO: Initialize audio processing pipeline
        self.is_monitoring = true;
        self.status_message = "Audio monitoring started".to_string();
        info!("Started audio monitoring");
        Ok(())
    }
```

With:

```rust
    async fn start_audio_monitoring(&mut self, _device: &str) -> Result<()> {
        // Audio pipeline setup is handled by the coordinator, which creates
        // audio → DSP → FT8 → TUI channels before launching the TUI.
        // This method just sets the monitoring flag so the UI knows to expect data.
        // Standalone TUI operation (without coordinator) is not yet supported.
        self.is_monitoring = true;
        self.status_message = "Audio monitoring active (via coordinator)".to_string();
        info!("Audio monitoring flag set — pipeline managed by coordinator");
        Ok(())
    }
```

- [ ] **Step 2: Run tests**

Run: `cargo check -p pancetta-tui`
Expected: Compiles with no errors

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "docs: clarify audio monitoring stub — pipeline managed by coordinator

Replace misleading TODO with documentation explaining that audio pipeline
setup is the coordinator's responsibility. The TUI flag correctly tracks
monitoring state for UI display purposes.

Co-Authored-By: Claude Opus 4.6 <noreply@anthropic.com>"
```

---

## Verification

### Task 9: Full workspace verification

- [ ] **Step 1: Run full workspace check**

Run: `cargo check --workspace --features transmit`
Expected: No errors

- [ ] **Step 2: Run full workspace tests**

Run: `cargo test --workspace --features transmit`
Expected: All tests pass

- [ ] **Step 3: Run clippy**

Run: `cargo clippy --workspace --features transmit -- -D warnings`
Expected: No warnings

- [ ] **Step 4: Verify no remaining dangerous unwraps**

Run a grep to confirm the specific dangerous unwraps are gone:
```bash
# These should return 0 matches in non-test code:
grep -n "Signals::new.*expect\|ctrl_c.*expect" pancetta/src/main.rs
grep -n "from_ymd_opt.*unwrap\|and_hms_opt.*unwrap" pancetta-qso/src/statistics.rs
grep -n "captures.get.*unwrap" pancetta-qso/src/exchange.rs
```

---

## Future TUI Work (Out of Scope)

Captured for tracking — not implemented in this plan:

- Mouse handling (scroll band activity, click callsigns)
- Help panel (F1 keyboard shortcut reference)
- DXCC entity lookup (country name, worked-before from logbook)
- DX priority scoring (DXCC entity, band/mode combos, contest, propagation)
- Audio level meter (real-time input level in station info)
- Device selection UI (enumerate and select audio devices)
- DSP/decoder performance stats (messages/min, decode latency)
- Standalone TUI mode (full pipeline without coordinator)
