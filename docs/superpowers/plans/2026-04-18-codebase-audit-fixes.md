# Pancetta Codebase Audit Fixes — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Fix all critical and high-priority bugs found in the 7-subsystem codebase audit, organized by priority tier and independently committable.

**Architecture:** Each task is a focused fix to one file or a small cluster of related files. Tasks are ordered by severity (critical safety/data-loss first, then sensitivity/reliability, then correctness). Tasks within a tier are independent and can be parallelized.

**Tech Stack:** Rust, tokio, crossbeam, rusqlite, cpal, rustfft, ratatui

---

## Phase 1: Critical Safety & Data Loss (C1–C11)

### Task 1: Fix message bus 1ms expiry dropping control messages

**Files:**
- Modify: `pancetta/src/message_bus.rs:437`

The message bus default timeout is 1000µs (1ms). Messages are timestamped at construction (`Instant::now()` in `ComponentMessage::new`), then checked with `is_expired()` in `send_message()`. Any message that takes >1ms between construction and routing is silently dropped. This breaks all inter-component control communication (PTT, QSO, transmit commands).

- [ ] **Step 1: Fix the timeout default**

In `pancetta/src/message_bus.rs`, change the default timeout from 1ms to 30 seconds:

```rust
// Line 437: change from
message_timeout_us: 1000, // 1ms timeout for real-time audio
// to
message_timeout_us: 30_000_000, // 30s timeout for control messages
```

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`
Expected: Clean build, no warnings.

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/message_bus.rs
git commit -m "fix: increase message bus expiry from 1ms to 30s

The 1ms timeout was designed for audio data but applied to all
messages including PTT, QSO, and transmit commands. These control
messages routinely take >1ms to route through the async runtime,
causing them to be silently dropped as expired."
```

---

### Task 2: Fix QsoCompleted event never emitted — auto-logging broken

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs:608-633`

The `QsoEvent::QsoCompleted` variant is defined but never emitted. `QsoLogger` and `AsyncQsoLogger` listen exclusively for this event to trigger auto-logging. No QSOs are ever automatically logged.

- [ ] **Step 1: Find the Completed state transition and add the event emission**

In `pancetta-qso/src/qso_manager.rs`, find where the state transitions to `QsoState::Completed` (around line 608-633). After the state change is applied, add:

```rust
// After: self.update_qso_state(&qso_id, new_state).await;
// Add QsoCompleted event emission:
if matches!(new_state, QsoState::Completed { .. }) {
    if let Some(progress) = self.get_qso(&qso_id).await.ok() {
        self.emit_event(QsoEvent::QsoCompleted {
            qso_id: qso_id.clone(),
            metadata: progress.metadata.clone(),
        }).await;
    }
}
```

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-qso 2>&1 | tail -5`
Expected: Clean build.

- [ ] **Step 3: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "fix: emit QsoCompleted event so auto-logging fires

QsoEvent::QsoCompleted was defined but never emitted. Both
QsoLogger and AsyncQsoLogger listen exclusively for this event
to trigger automatic QSO logging. Without it, completed QSOs
were silently dropped."
```

---

### Task 3: Fix overnight time restriction logic — inverted TX gate

**Files:**
- Modify: `pancetta-qso/src/auto_sequencer.rs:865-869`

The overnight branch (`start_hour > end_hour`, e.g., 22:00–06:00) has inverted logic: it allows TX during restricted hours and blocks during allowed hours.

- [ ] **Step 1: Fix the overnight condition**

In `pancetta-qso/src/auto_sequencer.rs`, replace lines 865-869:

```rust
// FROM:
} else {
    // Overnight restriction (e.g., 22:00 to 06:00)
    if hour < restrictions.start_hour && hour > restrictions.end_hour {
        return false;
    }
}

// TO:
} else {
    // Overnight window (e.g., start=22, end=06 means allowed 22:00-06:00)
    // Outside the window = hour < start AND hour > end
    if hour < restrictions.start_hour && hour > restrictions.end_hour {
        // Hour is between end and start (e.g., 07:00-21:59) — not in overnight window
        return false;
    }
}
```

Wait — re-reading the original: the function is `is_time_allowed()` and `return false` means "not allowed". The overnight range `start=22, end=6` means "allowed from 22:00 to 06:00". An hour like 3 (3 AM) should be allowed: `3 < 22 && 3 > 6` → `true && false` → `false`, so it doesn't return false → allowed. Correct. An hour like 10 (10 AM) should be blocked: `10 < 22 && 10 > 6` → `true && true` → `true`, so it returns false → blocked. Correct!

Actually, re-analyzing: the logic IS correct for the "allowed window" interpretation. The bug report may have the semantics backwards. Let me re-verify by reading the field names.

- [ ] **Step 1 (revised): Read the config field semantics**

Read `pancetta-qso/src/auto_sequencer.rs` around line 150-160 to check if `start_hour`/`end_hour` define the ALLOWED window or the RESTRICTED window. The field names and comments will clarify. If the fields define an allowed window (TX permitted between start and end), the current logic is correct. If they define a restricted window (TX forbidden between start and end), the logic is inverted.

Based on the field context, apply the appropriate fix. If the semantics are "allowed window" and the code is actually correct, skip this task.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-qso 2>&1 | tail -5`

- [ ] **Step 3: Commit if changed**

```bash
git add pancetta-qso/src/auto_sequencer.rs
git commit -m "fix: correct overnight time restriction logic in auto-sequencer"
```

---

### Task 4: Fix PTT watchdog clearing timer on failure

**Files:**
- Modify: `pancetta/src/coordinator.rs:1598-1617`

When the PTT safety watchdog fires and `set_ptt(Off)` fails (rigctld unreachable), it still clears `ptt_on_since`. The watchdog never retries, leaving PTT stuck on permanently.

- [ ] **Step 1: Only clear the timer on success**

In `pancetta/src/coordinator.rs`, replace lines 1598-1617:

```rust
// FROM:
if let Some(on_since) = ptt_time {
    if on_since.elapsed() > Duration::from_secs(PTT_SAFETY_TIMEOUT_SECS) {
        error!(
            "PTT SAFETY WATCHDOG: PTT has been on for >{} seconds — forcing OFF",
            PTT_SAFETY_TIMEOUT_SECS
        );
        if let Err(e) = rig_for_watchdog
            .set_ptt(
                pancetta_hamlib::Vfo::Current,
                pancetta_hamlib::PttState::Off,
            )
            .await
        {
            error!("PTT SAFETY WATCHDOG: failed to force PTT off: {}", e);
        } else {
            warn!("PTT SAFETY WATCHDOG: PTT forced off successfully");
        }
        // Clear the tracker so we don't keep firing
        let mut guard = ptt_watchdog_tracker.write().await;
        *guard = None;
    }
}

// TO:
if let Some(on_since) = ptt_time {
    if on_since.elapsed() > Duration::from_secs(PTT_SAFETY_TIMEOUT_SECS) {
        error!(
            "PTT SAFETY WATCHDOG: PTT has been on for >{} seconds — forcing OFF",
            PTT_SAFETY_TIMEOUT_SECS
        );
        match rig_for_watchdog
            .set_ptt(
                pancetta_hamlib::Vfo::Current,
                pancetta_hamlib::PttState::Off,
            )
            .await
        {
            Ok(_) => {
                warn!("PTT SAFETY WATCHDOG: PTT forced off successfully");
                // Only clear timer on success — retry on next tick if it fails
                let mut guard = ptt_watchdog_tracker.write().await;
                *guard = None;
            }
            Err(e) => {
                error!(
                    "PTT SAFETY WATCHDOG: failed to force PTT off: {} — will retry in 1s",
                    e
                );
            }
        }
    }
}
```

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "fix: PTT watchdog retries on failure instead of giving up

Previously, the watchdog cleared its timer regardless of whether
set_ptt(Off) succeeded. If rigctld was down at the critical moment,
PTT stayed on forever with no further retry. Now it only clears
the timer on success, retrying every 1s tick until PTT is confirmed off."
```

---

### Task 5: Fix async logger logging zero signal reports

**Files:**
- Modify: `pancetta-qso/src/async_logger.rs:385-403`

`handle_qso_completed` constructs `QsoProgress` with `their_report: 0, our_report: 0`, discarding the actual signal reports from the QSO state machine.

- [ ] **Step 1: Pass actual reports through the QsoCompleted event**

This requires the `QsoCompleted` event (fixed in Task 2) to carry the actual `QsoProgress`, not just `QsoMetadata`. In `pancetta-qso/src/qso_manager.rs`, update the event emission from Task 2 to include signal reports in the metadata (or add them to the event variant).

The simplest fix: in `async_logger.rs:handle_qso_completed`, fetch the full QSO state from the QSO manager instead of constructing a stub:

```rust
// In handle_qso_completed, replace the hardcoded QsoProgress construction with:
// First, try to get the actual QSO data from the database or event metadata
let their_report = metadata.get("their_report")
    .and_then(|v| v.parse::<i32>().ok())
    .unwrap_or(0);
let our_report = metadata.get("our_report")
    .and_then(|v| v.parse::<i32>().ok())
    .unwrap_or(0);
```

Note: The exact fix depends on what fields `QsoMetadata` carries. Read the struct definition to determine the right approach. The key requirement: signal reports must flow from `QsoState::Completed { their_report, our_report }` through to the logger.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-qso 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-qso/src/async_logger.rs pancetta-qso/src/qso_manager.rs
git commit -m "fix: pass actual signal reports through QsoCompleted event

handle_qso_completed was hardcoding their_report: 0, our_report: 0,
discarding the real SNR values from the QSO state machine."
```

---

### Task 6: Fix async database backup — use SQLite backup API

**Files:**
- Modify: `pancetta-qso/src/async_database.rs:314-334`

The async backup iterates all QSOs and reinserts them into a new database — non-atomic and corrupts on crash. The sync `QsoDatabase` correctly uses the SQLite online backup API.

- [ ] **Step 1: Replace export-reimport with SQLite backup API**

In `pancetta-qso/src/async_database.rs`, replace the `backup()` method body. Use `rusqlite::backup::Backup` (same as the sync version):

```rust
pub async fn backup(&self, path: &str) -> Result<()> {
    let backup_path = path.to_string();
    // Get a connection from the pool and run backup synchronously
    let pool = self.pool.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(|e| anyhow::anyhow!("Pool error: {}", e))?;
        let mut dst = rusqlite::Connection::open(&backup_path)?;
        let backup = rusqlite::backup::Backup::new(&conn, &mut dst)?;
        backup.run_to_completion(100, std::time::Duration::from_millis(10), None)?;
        Ok(())
    })
    .await?
}
```

Also add WAL mode in `initialize_schema()`:

```rust
// At the start of initialize_schema, add:
sqlx::query("PRAGMA journal_mode = WAL").execute(&self.pool).await?;
sqlx::query("PRAGMA synchronous = NORMAL").execute(&self.pool).await?;
```

Note: The exact approach depends on whether the async database uses `sqlx` or `r2d2`. Read the pool type to determine the right API.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-qso 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-qso/src/async_database.rs
git commit -m "fix: use SQLite backup API for atomic backups, enable WAL mode

The async backup was doing export-then-reimport which is non-atomic.
Crash mid-backup would corrupt the backup file. Now uses the SQLite
online backup API (same as sync version). Also enables WAL mode for
better concurrent read/write performance."
```

---

### Task 7: Add Drop impl for coordinator to kill rigctld on panic

**Files:**
- Modify: `pancetta/src/coordinator.rs:39-68`

If the coordinator panics during TX, the managed rigctld child process is never killed (no `Drop` impl). PTT could stay asserted.

- [ ] **Step 1: Add Drop impl**

After the `ApplicationCoordinator` struct definition in `pancetta/src/coordinator.rs`, add:

```rust
#[cfg(feature = "pancetta-hamlib")]
impl Drop for ApplicationCoordinator {
    fn drop(&mut self) {
        if let Some(mut child) = self.rigctld_process.take() {
            eprintln!("Pancetta: killing managed rigctld (PID {})", child.id());
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
```

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "fix: add Drop impl to kill rigctld on coordinator panic

If the coordinator panics during TX, the managed rigctld child
process was never cleaned up, potentially leaving PTT asserted."
```

---

### Task 8: Fix audio stream f32 assumption

**Files:**
- Modify: `pancetta-audio/src/stream.rs:295-320`

`build_input_stream` is hardcoded for `f32` samples. If the device config selects I16 or I32, cpal will panic or produce garbage.

- [ ] **Step 1: Check sample format and handle I16/I32**

In `pancetta-audio/src/stream.rs`, in `create_input_stream()`, after selecting the config, check the sample format and build the appropriate stream:

```rust
let sample_format = stream_config.sample_format();
let config: cpal::StreamConfig = stream_config.into();

let stream = match sample_format {
    cpal::SampleFormat::F32 => {
        device.build_input_stream(
            &config,
            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                producer.push_audio_slice(data);
            },
            |err| eprintln!("Input stream error: {}", err),
            None,
        )?
    }
    cpal::SampleFormat::I16 => {
        device.build_input_stream(
            &config,
            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                let float_data: Vec<f32> = data.iter()
                    .map(|&s| s as f32 / i16::MAX as f32)
                    .collect();
                producer.push_audio_slice(&float_data);
            },
            |err| eprintln!("Input stream error: {}", err),
            None,
        )?
    }
    format => {
        return Err(anyhow::anyhow!("Unsupported sample format: {:?}", format));
    }
};
```

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-audio 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-audio/src/stream.rs
git commit -m "fix: handle I16 sample format in audio input stream

build_input_stream was hardcoded for f32 samples. Devices that
report I16 as their preferred format would panic or produce garbage."
```

---

### Task 9: Fix SNR reporting — compute actual SNR instead of sync score

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:904-906`

The decoder reports `candidate.sync_score` as `snr_db`, which is a neighbor-comparison score, not actual SNR in 2.5kHz bandwidth.

- [ ] **Step 1: Compute proper SNR estimate**

In `pancetta-ft8/src/decoder.rs`, replace the sync_score assignment with a proper SNR estimate. The standard FT8 SNR estimate is: `SNR = 10*log10(signal_power / noise_power) - 10*log10(2500 / bandwidth_per_bin)`. A simpler approach matching ft8_lib: estimate signal and noise power from the spectrogram around the candidate's frequency.

```rust
// Replace:
let snr_db = candidate.sync_score as f32;

// With: Estimate SNR from the spectrogram power around the candidate
let signal_power: f64 = {
    let f0 = candidate.freq_bin;
    let t0 = candidate.time_step;
    let fs = candidate.freq_sub;
    // Average power across the 8 tone bins for data symbols
    let mut sum = 0.0;
    let mut count = 0;
    for sym in 0..self.protocol_params.num_symbols.min(20) {
        let t = t0 + sym * 2;
        if t < spectrogram.num_steps {
            for tone in 0..self.protocol_params.num_tones {
                if f0 + tone < spectrogram.num_bins {
                    sum += spectrogram.power[t][fs][f0 + tone];
                    count += 1;
                }
            }
        }
    }
    if count > 0 { sum / count as f64 } else { -120.0 }
};

// Noise estimate: average power outside the signal's tone range
let noise_power: f64 = {
    let t0 = candidate.time_step;
    let fs = candidate.freq_sub;
    let mut sum = 0.0;
    let mut count = 0;
    // Sample noise bins well away from the signal
    let noise_bins = [10, 20, 30, 40, 50];
    for &nb in &noise_bins {
        if nb < spectrogram.num_bins {
            let t = t0;
            if t < spectrogram.num_steps {
                sum += spectrogram.power[t][fs][nb];
                count += 1;
            }
        }
    }
    if count > 0 { sum / count as f64 } else { -120.0 }
};

// SNR in 2500 Hz reference bandwidth
// signal_power and noise_power are in log domain (dB), convert to linear
let snr_linear = 10.0f64.powf((signal_power - noise_power) / 10.0);
// Normalize to 2500 Hz bandwidth: SNR_2500 = SNR_bin * (bin_width / 2500)
let bin_width = SAMPLE_RATE as f64 / (self.fft_processor.fft_size() as f64);
let snr_db = (10.0 * (snr_linear * bin_width / 2500.0).log10()) as f32;
```

Note: This is an approximation. Read the spectrogram data format carefully — `power` values may already be in dB (log10) or linear. Adjust the computation accordingly.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-ft8 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "fix: compute actual SNR estimate instead of using Costas sync score

The decoder was reporting the Costas sync neighbor-comparison score
as snr_db, which has different units and range than actual SNR in
a 2500 Hz reference bandwidth. This affected autonomous operator
threshold decisions."
```

---

### Task 10: Fix hot-reload callback firing with stale config

**Files:**
- Modify: `pancetta-config/src/loader.rs:568-575`

The file watcher callback passes the pre-reload config to listeners, never loading the new file.

- [ ] **Step 1: Load the new config before invoking the callback**

In `pancetta-config/src/loader.rs`, in the file watcher thread (around line 568-575), after clearing the cache entry for the changed file, call the loader to read the new config before invoking the callback:

```rust
// After cache clear, before callback:
// Re-load config from all sources
if let Ok(new_config) = Self::load_from_sources(&sources_clone) {
    if let Ok(mut config_guard) = current_config.write() {
        *config_guard = new_config;
    }
}
// Now invoke callback with the updated config
if let Ok(callback_guard) = reload_callback.lock() {
    if let Some(ref callback) = *callback_guard {
        if let Ok(config_guard) = current_config.read() {
            callback(&*config_guard);
        }
    }
}
```

Note: Read the exact structure of the watcher thread to determine what reload mechanism is available. The key requirement: `current_config` must hold the NEW config when the callback fires.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-config 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-config/src/loader.rs
git commit -m "fix: reload config before invoking hot-reload callback

The file watcher was passing the pre-reload (stale) config to
callback listeners. Now loads the new config first."
```

---

### Task 11: Fix DxTracker thread safety — add Mutex around Connection

**Files:**
- Modify: `pancetta-dx/src/tracker.rs:89-91`

`DxTracker` wraps a bare `rusqlite::Connection` in `Arc` without a `Mutex`. Concurrent async access causes data races.

- [ ] **Step 1: Wrap Connection in Mutex**

In `pancetta-dx/src/tracker.rs`, change:

```rust
// FROM:
pub struct DxTracker {
    connection: Connection,
}

// TO:
pub struct DxTracker {
    connection: tokio::sync::Mutex<Connection>,
}
```

Then update all methods that access `self.connection` to acquire the lock first:

```rust
// Every method that uses self.connection changes from:
self.connection.execute(...)
// To:
let conn = self.connection.lock().await;
conn.execute(...)
```

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-dx 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-dx/src/tracker.rs
git commit -m "fix: wrap DxTracker Connection in Mutex for thread safety

The bare Connection was shared via Arc across async tasks without
synchronization. rusqlite::Connection is Send but not Sync."
```

---

## Phase 2: Sensitivity & Reliability (H1–H25)

### Task 12: Fix Costas sync — use both half-steps for 3 dB sensitivity gain

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:684-710`

The Costas sync search only examines the first of two time steps per symbol, losing 3 dB of sync detection energy.

- [ ] **Step 1: Average both half-steps**

In `pancetta-ft8/src/decoder.rs`, in `compute_costas_score()`, change the time index computation to average both half-steps:

```rust
// Replace line 686:
let time_idx = t0 + symbol_idx * 2;

// With: average both half-symbol steps
for half in 0..2 {
    let time_idx = t0 + symbol_idx * 2 + half;

    if time_idx >= spec.num_steps {
        continue;
    }

    // ... rest of the scoring body stays the same, but inside this loop
}
```

The full change: wrap the existing scoring body (lines 686-730) in a `for half in 0..2` loop, computing `time_idx = t0 + symbol_idx * 2 + half` for each iteration. The `score` and `num_average` accumulators naturally absorb both halves.

- [ ] **Step 2: Build and run decoder tests**

Run: `cargo test -p pancetta-ft8 2>&1 | tail -20`

- [ ] **Step 3: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: use both half-symbol steps in Costas sync for +3 dB sensitivity

The sync search was only examining the first of two time steps per
symbol, discarding half the available sync energy. Averaging both
half-steps improves sync detection SNR by ~3 dB."
```

---

### Task 13: Fix Hamlib connection desync — dedicated command task

**Files:**
- Modify: `pancetta-hamlib/src/rigctld.rs:93-175`

Multiple issues: `send_command` reads only one line (multi-line responses desync), failed commands don't set `connected=false` (reconnection never triggers), and concurrent tasks can interleave during retry windows.

- [ ] **Step 1: Fix send_command to handle errors and set connected=false**

In `pancetta-hamlib/src/rigctld.rs`, in `send_command()`, when the read returns EOF or an error, set the connection state:

```rust
// After the match on read_line result, in the error arms:
Ok(Ok(0)) => {
    // Connection closed
    let mut state = self.state.write().await;
    state.connected = false;
    *self.conn.lock().await = None;
    Err(anyhow!("rigctld closed connection"))
}
Ok(Err(e)) => {
    let mut state = self.state.write().await;
    state.connected = false;
    *self.conn.lock().await = None;
    Err(anyhow!("Failed to read response: {}", e))
}
Err(_) => {
    let mut state = self.state.write().await;
    state.connected = false;
    *self.conn.lock().await = None;
    Err(anyhow!("Command timeout"))
}
```

- [ ] **Step 2: Fix get_mode to read two lines**

In `get_mode()`, rigctld returns mode and passband on separate lines. Read both:

```rust
async fn get_mode(&self, vfo: Vfo) -> Result<(Mode, i32)> {
    let cmd = "m"; // short-form get mode
    let mode_str = self.send_command_with_retry(cmd).await?;
    let passband_str = self.send_command_with_retry("").await
        .unwrap_or_else(|_| "0".to_string());
    // ... parse mode_str and passband_str
}
```

Actually, a cleaner approach: add a `send_command_multiline` that reads N lines. But the simplest fix is to just read the second line after the first `send_command` returns, while still holding the conn lock. This requires modifying `send_command` to optionally read additional lines, or adding a `read_line` helper.

Note: Read the exact `get_mode` implementation to determine the best approach. The key requirement: both the mode line AND the passband line must be consumed from the BufReader.

- [ ] **Step 3: Remove or fix get_info (dump_state)**

`get_info()` calls `\dump_state` which returns dozens of lines. Either:
- Remove `get_info()` if unused, or
- Replace with a simpler command (`\get_info` returns a single line on some hamlib versions), or
- Add a multi-line reader that reads until an empty line or RPRT

- [ ] **Step 4: Build and verify**

Run: `cargo build -p pancetta-hamlib 2>&1 | tail -5`

- [ ] **Step 5: Commit**

```bash
git add pancetta-hamlib/src/rigctld.rs
git commit -m "fix: rigctld connection reliability — error recovery, multi-line responses

- Set connected=false and clear conn on send_command errors so
  retry logic triggers reconnection
- Handle two-line get_mode response (mode + passband)
- Fix get_info to not desync the connection with dump_state output"
```

---

### Task 14: Fix PTT-on during cancelled transmitter task

**Files:**
- Modify: `pancetta/src/coordinator.rs` (transmitter task, around line 2300-2382)

If the transmitter task is cancelled (Ctrl+C during TX), PTT-off is never sent. The 30s watchdog is the only backstop, and it may also fail (see Task 4).

- [ ] **Step 1: Send PTT-off before the sleep, not after**

Restructure the transmit sequence to ensure PTT-off is always sent. Use a guard pattern:

```rust
// Before the transmit sleep, create a guard that sends PTT-off on drop:
struct PttGuard {
    rig: Arc<Box<dyn RigControl + Send + Sync>>,
    rt: tokio::runtime::Handle,
}

impl Drop for PttGuard {
    fn drop(&mut self) {
        let rig = self.rig.clone();
        let _ = self.rt.block_on(async {
            let _ = rig.set_ptt(Vfo::Current, PttState::Off).await;
        });
    }
}

// Usage:
let _ptt_guard = PttGuard { rig: rig.clone(), rt: Handle::current() };
rig.set_ptt(Vfo::Current, PttState::On).await?;
// ... sleep for audio duration ...
// PttGuard::drop runs automatically, even on task cancellation
```

Note: Read the transmitter task code to understand the exact structure. The guard must be created BEFORE `set_ptt(On)` and dropped AFTER the audio completes (or on cancellation).

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "fix: ensure PTT-off is sent even if transmitter task is cancelled

Uses a Drop guard so PTT-off is always sent, whether the task
completes normally or is cancelled during shutdown."
```

---

### Task 15: Fix mem::forget on log WorkerGuard

**Files:**
- Modify: `pancetta/src/main.rs:1048-1101`

`mem::forget(_guard)` prevents the log appender from flushing on exit. Buffered log records are lost on crash.

- [ ] **Step 1: Return the guard from init_logging and hold it in main**

```rust
// Change init_logging signature to return the guard:
fn init_logging(cli: &Cli, headless: bool) -> Result<WorkerGuard> {
    // ... existing setup ...
    
    // Remove: std::mem::forget(_guard);
    // Instead, return the guard:
    Ok(guard)
}

// In main(), hold the guard:
let _log_guard = init_logging(&cli, cli.headless)?;
// ... rest of main ...
// _log_guard drops here, flushing all buffered logs
```

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/main.rs
git commit -m "fix: hold log WorkerGuard in main instead of mem::forget

mem::forget bypassed the Drop impl that flushes buffered log records.
Crash logs were silently lost. Now the guard lives in main and
flushes properly on normal or abnormal exit."
```

---

### Task 16: Fix sample rate validation — reject non-multiples of 12000

**Files:**
- Modify: `pancetta/src/coordinator.rs:722`

44100 Hz input → decimation factor 3 → output 14700 Hz (not 12000). FT8 decode fails silently.

- [ ] **Step 1: Add validation**

In the DSP pipeline setup (around line 722):

```rust
let decimation_factor = (input_rate / 12000) as usize;
// Add after:
if input_rate as usize != decimation_factor * 12000 {
    return Err(anyhow::anyhow!(
        "Audio sample rate {} Hz is not evenly divisible by 12000 Hz. \
         Supported rates: 12000, 24000, 48000, 96000.",
        input_rate
    ));
}
```

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "fix: reject sample rates not divisible by 12000 Hz

44100 Hz would produce decimation factor 3 → output 14700 Hz,
not the 12000 Hz the FT8 decoder expects. All frequency
references would be wrong and decoding would fail silently."
```

---

### Task 17: Fix TUI bare 'q' quit — require Ctrl+Q

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs:322-325`

Pressing 'q' immediately quits even when typing a callsign in the TX buffer.

- [ ] **Step 1: Change quit to Ctrl+Q only**

In `pancetta-tui/src/tui_runner.rs`, replace the quit handler:

```rust
// FROM:
KeyCode::Char('q') | KeyCode::Char('Q') => {
    let _ = self.message_tx.send(TuiCommand::Quit);
    return Ok(false);
}

// TO: (only quit on Ctrl+Q, not bare q)
KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL) => {
    let _ = self.message_tx.send(TuiCommand::Quit);
    return Ok(false);
}
```

Also update the status bar hint to show `Ctrl+Q` instead of `Q:Quit`.

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs
git commit -m "fix: require Ctrl+Q to quit, not bare q

Bare 'q' was quitting the app while the user was typing callsigns
containing the letter Q in the TX input buffer."
```

---

### Task 18: Fix ADIF export — MODE=FT8, proper field separators, correct signal reports

**Files:**
- Modify: `pancetta-qso/src/adif.rs:393-400, 795-806, 929-940`

Three ADIF issues: MODE="DATA" instead of "FT8", no newline between fields, and SNR→RST bucketing destroys precision.

- [ ] **Step 1: Fix MODE field**

```rust
// FROM:
mode: if metadata.mode == "FT8" {
    "DATA".to_string()
} else { metadata.mode.clone() },

// TO:
mode: metadata.mode.clone(), // ADIF 3.1 supports "FT8" directly
submode: None, // FT8 is a first-class mode, no submode needed
```

- [ ] **Step 2: Add newline after each field**

In `format_field()`, append a space or newline after each field:

```rust
// Change format_field to append a space:
fn format_field(name: &str, value: &str) -> String {
    format!("<{}:{}>{} ", name, value.len(), value)
}
```

And ensure each record ends with `<EOR>\n`.

- [ ] **Step 3: Fix signal report — store raw SNR**

```rust
// Replace signal_report_to_rst with direct formatting:
fn format_signal_report(snr: i32) -> String {
    format!("{:+03}", snr) // e.g., "-21", "+03"
}
```

- [ ] **Step 4: Build and verify**

Run: `cargo build -p pancetta-qso 2>&1 | tail -5`

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/adif.rs
git commit -m "fix: ADIF export — correct MODE, field separators, signal reports

- MODE=FT8 (not DATA+SUBMODE) per ADIF 3.1.4
- Add space separator between fields for LoTW compatibility
- Store raw SNR dB values instead of bucketed RST codes"
```

---

### Task 19: Fix QsoLogger clone creating orphaned state

**Files:**
- Modify: `pancetta-qso/src/logger.rs:1009` (or wherever `Clone` is implemented)

`QsoLogger::Clone` creates fresh empty QSO maps. Background tasks use the clone and can never find QSOs.

- [ ] **Step 1: Change QsoLogger to use Arc<Self> pattern**

Replace `self.clone()` in `start()` with `Arc::new(self)` or pass shared references. The exact approach depends on how `QsoLogger` is structured. Read the `start()` method and the Clone impl to determine the right fix.

The key requirement: background tasks spawned by `start()` must share the same QSO maps as the original `QsoLogger`, not get empty copies.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-qso 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-qso/src/logger.rs
git commit -m "fix: QsoLogger background tasks share state via Arc, not Clone

Clone was creating independent empty QSO maps for background tasks.
get_qso() always returned QsoNotFound in the spawned task context."
```

---

### Task 20: Fix waterfall channel — separate live and per-cycle channels

**Files:**
- Modify: `pancetta/src/coordinator.rs:486, 502, 506, 511`

Both DSP (live rows) and FT8 (cycle matrices) send to the same waterfall channel. The TUI has no way to distinguish them, and `Receiver::clone()` creates competing consumers.

- [ ] **Step 1: Use a single sender path — remove the clone issue**

The simplest fix: don't clone `waterfall_rx`. Pass the original to the TUI pipeline and remove the clone:

```rust
// Line 511: change from
self.start_tui_pipeline(ft8_to_tui_rx, tui_bus_rx, waterfall_rx.clone())

// To:
self.start_tui_pipeline(ft8_to_tui_rx, tui_bus_rx, waterfall_rx)
```

The original `waterfall_rx` is not used after this point, so passing ownership (not cloning) eliminates the competing consumer issue.

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "fix: pass waterfall_rx by ownership, not clone

Cloning the crossbeam receiver created competing consumers.
The TUI was only seeing a subset of waterfall rows."
```

---

### Task 21: Fix dual SIGINT handlers

**Files:**
- Modify: `pancetta/src/main.rs:291-319`

Two separate signal handlers both fire on Ctrl+C. Remove the `signal_hook_tokio` one and keep `tokio::signal::ctrl_c()`.

- [ ] **Step 1: Remove signal_hook handler, keep tokio::signal::ctrl_c()**

In `pancetta/src/main.rs`, remove the `signal_hook_tokio::Signals` handler (around lines 291-310) and keep only the `tokio::signal::ctrl_c()` handler. Also remove the `signal-hook-tokio` import if no longer used.

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/main.rs
git commit -m "fix: remove duplicate SIGINT handler — keep tokio::signal::ctrl_c()

Two handlers both fired on Ctrl+C, potentially conflicting.
tokio::signal::ctrl_c() is sufficient and portable."
```

---

### Task 22: Unify TUI render paths

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs:496-563`

The integrated app uses inline render methods in `tui_runner.rs` (bare List-based band activity), while the standalone binary uses the rich Table-based UI in `ui/`. The operator never sees the rich UI.

- [ ] **Step 1: Replace tui_runner inline rendering with calls to ui::draw**

In `pancetta-tui/src/tui_runner.rs`, replace `render_main_content_static` and its helper methods with a call to the `ui::draw` function:

```rust
fn render_main_content_static(f: &mut Frame, area: Rect, app: &App) {
    // Delegate to the rich UI module
    if let Err(e) = crate::ui::draw(f, area, app) {
        // Fallback: show error
        let msg = Paragraph::new(format!("Render error: {}", e));
        f.render_widget(msg, area);
    }
}
```

Note: The `ui::draw` function signature may not match exactly. Read both the current `render_main_content_static` and `ui::draw` to determine what adapter code is needed. The key requirement: the integrated app must use the same rich rendering as the standalone binary.

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs
git commit -m "feat: unify TUI render paths — integrated app uses rich UI

The integrated app was using bare inline rendering while the standalone
binary had rich Table-based band activity, station info panel, and DX
hunter. Now both use the same ui::draw path."
```

---

### Task 23: Fix DX cluster login — send callsign on prompt

**Files:**
- Modify: `pancetta-dx/src/cluster.rs:273-287`

The telnet reader detects the login prompt but never sends the callsign. Servers timeout and disconnect.

- [ ] **Step 1: Send callsign when login prompt is detected**

In `pancetta-dx/src/cluster.rs`, in the reader task's login prompt detection:

```rust
// Replace the comment-only handler with actual login:
if line.contains("call:")
    || line.contains("callsign:")
    || line.contains("Please enter your call")
{
    info!("Login prompt detected — sending callsign");
    if let Err(e) = cmd_tx.send(callsign.clone()) {
        error!("Failed to send callsign for login: {}", e);
    }
}
```

Note: Check that `callsign` is captured in the closure (the `_callsign` variable at line 233 is suppressed with `_`). Remove the underscore prefix to use it.

- [ ] **Step 2: Build and verify**

Run: `cargo build -p pancetta-dx 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add pancetta-dx/src/cluster.rs
git commit -m "fix: send callsign when DX cluster login prompt is detected

The login prompt was detected but the callsign was never sent,
causing DX cluster servers to timeout and disconnect."
```

---

## Phase 3: Correctness & Robustness (Medium Priority)

### Task 24: Fix division by zero in audio level calculation

**Files:**
- Modify: `pancetta-tui/src/app.rs:479`

- [ ] **Step 1: Add empty-data guard**

```rust
// Add before the RMS calculation:
if data.is_empty() {
    return Ok(());
}
```

- [ ] **Step 2: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "fix: guard against division by zero on empty audio buffer"
```

---

### Task 25: Fix scroll Rect u16 underflow

**Files:**
- Modify: `pancetta-tui/src/ui/band_activity.rs:100-105`
- Modify: `pancetta-tui/src/ui/dx_hunter.rs:92-97`

- [ ] **Step 1: Use saturating_sub**

```rust
// Change:
x: area.x + area.width - scroll_info.len() as u16 - 2,
// To:
x: area.x + area.width.saturating_sub(scroll_info.len() as u16 + 2),
```

Apply to both files.

- [ ] **Step 2: Commit**

```bash
git add pancetta-tui/src/ui/band_activity.rs pancetta-tui/src/ui/dx_hunter.rs
git commit -m "fix: prevent u16 underflow in scroll indicator at small terminal sizes"
```

---

### Task 26: Fix waterfall unit test assertions

**Files:**
- Modify: `pancetta-tui/src/widgets/mod.rs:563-569`

Tests assert old color scheme (Blue/Green/Red) but implementation now uses grayscale.

- [ ] **Step 1: Update test assertions**

```rust
#[test]
fn test_waterfall_color_intensity() {
    let waterfall = Waterfall::new(&[]);
    // Classic scheme uses grayscale ramp (indexed 232-255)
    assert!(matches!(waterfall.get_color_for_intensity(0.1), Color::Indexed(234..=236)));
    assert!(matches!(waterfall.get_color_for_intensity(0.5), Color::Indexed(243..=244)));
    assert!(matches!(waterfall.get_color_for_intensity(0.9), Color::Indexed(252..=253)));
}
```

- [ ] **Step 2: Run tests**

Run: `cargo test -p pancetta-tui 2>&1 | tail -10`

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/widgets/mod.rs
git commit -m "fix: update waterfall color test assertions for grayscale scheme"
```

---

### Task 27: Fix get_selected_station frequency unit mismatch

**Files:**
- Modify: `pancetta-tui/src/app.rs:789-792`

`msg.frequency * 1_000_000 + msg.delta_freq` incorrectly adds audio offset Hz to dial Hz.

- [ ] **Step 1: Fix frequency calculation**

```rust
// The CallStation command should receive the dial frequency in Hz
// delta_freq is the audio sub-band offset, not added to dial
let freq_hz = (msg.frequency * 1_000_000.0) as u64;
```

- [ ] **Step 2: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "fix: get_selected_station returns dial frequency without audio offset"
```

---

### Task 28: Fix live waterfall underflow after buffer drain

**Files:**
- Modify: `pancetta/src/coordinator.rs:839`

`ft8_buffer.len() - last_live_wf_samples` can underflow after the buffer is drained.

- [ ] **Step 1: Use saturating_sub**

```rust
// Change:
let samples_since_last = ft8_buffer.len() - last_live_wf_samples;
// To:
let samples_since_last = ft8_buffer.len().saturating_sub(last_live_wf_samples);
```

- [ ] **Step 2: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "fix: prevent subtraction underflow in live waterfall sample tracking"
```

---

### Task 29: Fix FT8 window time resync after slow decode

**Files:**
- Modify: `pancetta/src/coordinator.rs:897`

`next_window_time += 15s` doesn't resync to UTC boundary after a late decode.

- [ ] **Step 1: Resync to next future boundary**

```rust
// Replace:
next_window_time = next_window_time + chrono::Duration::seconds(15);

// With:
let now = chrono::Utc::now();
let secs = now.timestamp() % 15;
let wait_secs = if secs == 0 { 15 } else { 15 - secs };
next_window_time = now + chrono::Duration::seconds(wait_secs);
```

- [ ] **Step 2: Commit**

```bash
git add pancetta/src/coordinator.rs
git commit -m "fix: resync FT8 window timer to next UTC boundary after slow decode

Fixed +15s advancement caused rapid-fire window sends after a slow
decode cycle. Now recomputes the next 15-second UTC boundary."
```

---

### Task 30: Delete dead logging.rs module

**Files:**
- Delete: `pancetta/src/logging.rs`
- Modify: `pancetta/src/main.rs` (remove `mod logging` if present)

The module is never used — `main.rs` has its own inline `init_logging`.

- [ ] **Step 1: Remove the module**

Delete `pancetta/src/logging.rs` and remove `mod logging;` from `main.rs` (or `lib.rs`).

- [ ] **Step 2: Build and verify**

Run: `cargo build --bin pancetta 2>&1 | tail -5`

- [ ] **Step 3: Commit**

```bash
git add -A pancetta/src/logging.rs pancetta/src/main.rs pancetta/src/lib.rs
git commit -m "chore: remove dead logging.rs module — duplicated by main.rs::init_logging"
```

---

### Task 31: Fix frequency delta threshold — increase to 2 kHz

**Files:**
- Modify: `pancetta-tui/src/ui/station_info.rs:145-160`

500 Hz threshold triggers the red alarm too easily for normal crystal tolerance.

- [ ] **Step 1: Change threshold**

```rust
// Change:
if delta_khz.abs() > 0.5 {
// To:
if delta_khz.abs() > 2.0 {
```

- [ ] **Step 2: Commit**

```bash
git add pancetta-tui/src/ui/station_info.rs
git commit -m "fix: increase frequency delta warning threshold from 500 Hz to 2 kHz"
```

---

### Task 32: Fix status bar key hint mismatch

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs`

Status bar shows `</>:TXfreq` but actual keys are `[`/`]` and arrow keys.

- [ ] **Step 1: Update hint text**

```rust
// Change status bar format to:
" Decoded: {} | TX {:.0}Hz | Arrows:TXfreq +/-:Band Ctrl+Q:Quit "
```

- [ ] **Step 2: Update waterfall title similarly**

```rust
// Change waterfall title to match:
let title = format!(" Waterfall | TX {:.0} Hz ", app.tx_frequency_offset);
```

- [ ] **Step 3: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs
git commit -m "fix: correct key hint labels in status bar and waterfall title"
```

---

### Task 33: Fix Eastern hemisphere coordinate display

**Files:**
- Modify: `pancetta-tui/src/ui/station_info.rs:276`

Negative sign + "°W" shows "-9.00°W" for European stations.

- [ ] **Step 1: Fix sign convention**

```rust
// Replace:
format!("{:.2}°N, {:.2}°W", lat, -lon)
// With:
format!("{:.2}°{}, {:.2}°{}",
    lat.abs(), if lat >= 0.0 { "N" } else { "S" },
    lon.abs(), if lon >= 0.0 { "E" } else { "W" },
)
```

- [ ] **Step 2: Commit**

```bash
git add pancetta-tui/src/ui/station_info.rs
git commit -m "fix: correct coordinate display for all hemispheres"
```

---

## Phase 4: Stub Implementations (to be tracked separately)

These are large features that need their own design/plan cycles, not simple bug fixes:

- **H21**: DXCC database is a 5-entry stub — needs CTY.dat parser or bundled database
- **H23**: All DX statistics return hardcoded zeros — needs real SQL queries
- **M23**: `get_activity_timeline` returns fake data — needs real implementation or removal
- **M5**: Coordinator decomposition (3285 lines) — needs architectural plan
- **H14 alternative**: If ui::draw can't be directly wired (Task 22), the rich UI modules need to be ported to tui_runner's rendering approach

These should be filed as separate specs after the bug fixes are complete.

---

## Execution Order Summary

**Phase 1 (Critical):** Tasks 1–11 — do these first, in any order (all independent)
**Phase 2 (High):** Tasks 12–23 — do these next, in any order
**Phase 3 (Medium):** Tasks 24–33 — cleanup and correctness
**Phase 4 (Stubs):** Separate planning cycle needed
