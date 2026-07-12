# Audio Auto-Recovery Supervisor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close `docs/audio-robustness-plan.md`'s remaining §Design gaps (items 1 and 4; item 2's
primitive and item 3's wiring already shipped in PR #85) so a disconnected, replugged, or silently
wedged USB audio codec recovers automatically instead of leaving the station dead until a manual
restart.

**Architecture:** The audio thread (`pancetta/src/coordinator/audio.rs`, a dedicated `std::thread`
owning the non-`Send` cpal streams via `AudioManager`) already has a proven, tested reopen primitive
(`AudioManager::reopen_devices`) that today only fires on an operator-triggered `AudioReopenRequest`
from the TUI device picker. This plan adds two independent triggers for the *same* primitive:
(1) the audio thread's own `Err` arm, on a real `StreamError`, drives a capped-exponential backoff
loop that calls `reopen_devices` directly; (2) the async relay task's existing 2s staleness timeout
(today purely a reporting flag) additionally sends a synthetic, force-flagged `AudioReopenRequest`
through the *existing* `reopen_tx` channel when a device goes silently wedged (no error, no data).
Both triggers ultimately funnel into loop-body code that runs on a single thread, so there is no new
locking or coordination primitive to build — the existing single-threaded loop already serializes
every call to `reopen_devices`, including the operator's manual picker requests.

**Tech Stack:** Rust, `std::thread` + `tokio::task` (audio thread / relay task split, unchanged),
`crossbeam_channel` (existing `reopen_tx`/`reopen_rx`), `cpal` (via `AudioManager`, untouched by this
plan), the existing `MessageType::DiagnosticEvent` bus (PR #84).

## Global Constraints

- Never touch cpal streams from any thread other than the dedicated audio thread — `AudioStreamManager`
  is not `Send`. All recovery logic that calls `reopen_devices` directly must run in that thread's
  loop body; anything triggered from the relay task (a separate tokio task) must go through the
  existing `reopen_tx` channel, never call into `AudioManager` directly.
- No new dependencies.
- No CPU hot-spin: every retry path must sleep or block on a channel/timeout before looping.
- `cargo build` and `cargo test -p pancetta -p pancetta-audio` must pass after every task.
- Existing tests are hardware-gated with `if let Ok(manager) = AudioManager::new() { ... }` on a
  headless CI box (no real audio device) — follow this pattern for any new test that touches
  `AudioManager` directly; keep new pure-logic unit tests hardware-independent (no `AudioManager`
  involved at all, matching `resolve_reopen_targets`'s existing test style in `manager.rs`).
- Per this repo's `CLAUDE.md` Documentation Policy: update inline `///`/`//!` docs on every modified
  public item, and append a dated entry to `docs/DECISIONS/config-and-platform.md` (Task 5).

---

## Key findings from reading the current code (do not re-derive — trust these, but re-grep line
numbers before editing since this file churns)

1. **`reopen_devices(None, None, force)` requires `force: true` for same-device recovery.**
   `resolve_reopen_targets` (`pancetta-audio/src/manager.rs`) returns `None` whenever both
   `input`/`output` requests are `None` — *before* `force` is even consulted for the "unchanged"
   short-circuit path. With `force: false`, `reopen_devices(None, None, false)` on an unchanged
   device is a **silent no-op** (`"Audio reopen requested but selection is unchanged — no-op"`,
   returns `Ok(())` without touching the stream at all). Auto-recovery has no new device name to
   supply — it always wants "rebuild the same device" — so every recovery-triggered call in this
   plan **must** pass `force: true`, exactly like the existing "Jump Desktop reclaim" operator flow.
   Getting this wrong makes the whole feature silently do nothing while still logging as if it
   succeeded — this is the single easiest way to submit a plausible-looking regression on this task,
   so each task below states `force: true` explicitly and Task 2 adds a regression test for it.
2. **`reopen_devices`'s `Result` already IS the success/failure signal** — no need to also inspect
   `has_stream_error()` after calling it. On failure it always returns the original `Err` (even when
   rollback to the prior device succeeds) and always leaves the stream live on *some* working
   configuration or genuinely down; on success it returns `Ok(())` and `self.shared` has already been
   replaced with a fresh, non-errored `AudioCommShared` (`apply_devices_to_stream`,
   `pancetta-audio/src/manager.rs`, replaces `self.shared` unconditionally on every reopen — old
   `stream_error` latches on the discarded `AudioCommShared` are simply orphaned, not read again).
   `AudioCommShared::clear_stream_error` (added in PR #85) is therefore not needed by this plan's
   recovery path — do not add a call to it; it stays available for any future in-place-without-reopen
   recovery.
3. **The audio thread's loop is single-threaded and already serializes every `reopen_devices` call.**
   The loop body runs, in order, every iteration: drain `reopen_rx` (operator-triggered requests) →
   check TX audio → call `process_audio()`. Because this plan's Err-arm recovery (Task 2) also calls
   `reopen_devices` directly from *inside* that same loop body, and this plan's watchdog trigger
   (Task 3) is delivered through the *same* `reopen_rx` channel the operator picker already uses,
   there is never a genuine data race between an operator's manual device switch and either recovery
   path — worst case is one redundant `reopen_devices` call in an adjacent iteration (harmless: it
   just rebuilds the already-working stream again). **Do not add a mutex, atomic "recovery in
   progress" flag, or other coordination primitive for this** — it isn't needed, and the doc's
   "Don't fight the operator" risk note is satisfied by the existing single-threaded loop structure.
4. **`AudioManagerStats.underruns`/`.overruns` are always zero** — declared in
   `pancetta-audio/src/manager.rs` but never incremented anywhere in the crate. Do **not** surface
   these fields (doing so would be reporting fake data). The real, incremented counters are
   `AudioCommShared.dropped_samples` / `.processed_samples` (bumped in `push_audio_slice`,
   `pancetta-audio/src/ringbuffer_comm.rs`), which have no public accessor yet on `AudioManager`
   itself — Task 4 adds one.
5. **Output-side loss detection (doc item 3) is already done** (PR #85, `stream.rs` line ~580,
   `err_shared_output.set_stream_error()`) and shares the *same* flag as the input side — confirmed
   still true against current code. No work needed; Task 2's recovery loop covers both sides for
   free since both set the same `stream_error` flag that `process_audio()` checks.

## File Structure

- **Create `pancetta/src/coordinator/audio_recovery.rs`** — pure, hardware-independent logic:
  `RecoveryBackoff` (capped-exponential backoff state machine for the Err-arm loop) and
  `StaleWatchdog` (edge-detects "just went stale" vs. "still stale" for the relay task, so the
  watchdog fires a reopen request once per stale episode instead of every 2s tick). Neither type
  touches `AudioManager`, cpal, or tokio — fully unit-testable.
- **Modify `pancetta/src/coordinator/audio.rs`** — wire `RecoveryBackoff` into the audio thread's
  `Err` arm (Task 2) and `StaleWatchdog` + a cloned `reopen_tx` into the relay task's stale-timeout
  arm (Task 3); add periodic drop-rate diagnostic emission (Task 4).
- **Modify `pancetta/src/coordinator/mod.rs`** — add `mod audio_recovery;` alongside the existing
  `mod audio;` and friends (Task 1).
- **Modify `pancetta-audio/src/manager.rs`** — add `AudioManager::dropped_samples()` /
  `AudioManager::drop_rate_percent()` accessors (Task 4).
- **Modify `docs/audio-robustness-plan.md`** and **`docs/DECISIONS/config-and-platform.md`** — status
  updates (Task 5).

---

### Task 1: `RecoveryBackoff` and `StaleWatchdog` pure logic

**Files:**
- Create: `pancetta/src/coordinator/audio_recovery.rs`
- Modify: `pancetta/src/coordinator/mod.rs:19` (add `mod audio_recovery;` after `mod audio;`)

**Interfaces:**
- Produces (consumed by Task 2): `pub struct RecoveryBackoff` with `pub fn new() -> Self`,
  `pub fn next_delay(&mut self) -> std::time::Duration`, `pub fn reset(&mut self)`,
  `pub fn attempts(&self) -> u32`.
- Produces (consumed by Task 3): `pub struct StaleWatchdog` with `pub fn new() -> Self`,
  `pub fn on_timeout(&mut self) -> bool` (returns `true` the first time it's called after
  construction or after `on_data`, `false` on every subsequent call while still stale — this is the
  edge-detector), `pub fn on_data(&mut self)` (resets the edge so the next `on_timeout` fires again).

- [ ] **Step 1: Write the failing tests for `RecoveryBackoff`**

Create `pancetta/src/coordinator/audio_recovery.rs`:

```rust
//! Pure, hardware-independent recovery-decision logic for the audio thread.
//!
//! Kept separate from `audio.rs` (which owns the actual cpal/thread wiring)
//! so the backoff and watchdog-edge-detection policies are unit-testable
//! without a real `AudioManager`.

use std::time::Duration;

/// Capped-exponential backoff for repeated `reopen_devices` attempts after a
/// `process_audio` error. The first call after construction (or after a
/// [`reset`](Self::reset)) returns [`Duration::ZERO`] — a StreamError should
/// trigger an immediate reopen attempt, not a wasted sleep, since the common
/// case (a brief USB blip) recovers on the very first try. Only a *failed*
/// attempt should pay the backoff delay before the next one.
pub struct RecoveryBackoff {
    delay: Duration,
    attempts: u32,
}

const INITIAL_DELAY: Duration = Duration::from_millis(250);
const MAX_DELAY: Duration = Duration::from_secs(5);

impl RecoveryBackoff {
    pub fn new() -> Self {
        Self {
            delay: Duration::ZERO,
            attempts: 0,
        }
    }

    /// Returns the delay to sleep before the *next* reopen attempt, then
    /// advances the internal schedule (doubling, capped at [`MAX_DELAY`]) and
    /// increments the attempt counter.
    pub fn next_delay(&mut self) -> Duration {
        let d = self.delay;
        self.delay = if self.delay.is_zero() {
            INITIAL_DELAY
        } else {
            (self.delay * 2).min(MAX_DELAY)
        };
        self.attempts += 1;
        d
    }

    /// Reset after a successful recovery — the next `next_delay()` call will
    /// again return `Duration::ZERO` (immediate retry on the next failure).
    pub fn reset(&mut self) {
        self.delay = Duration::ZERO;
        self.attempts = 0;
    }

    /// How many attempts have been made since construction/the last reset.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }
}

impl Default for RecoveryBackoff {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_delay_is_immediate() {
        let mut b = RecoveryBackoff::new();
        assert_eq!(b.next_delay(), Duration::ZERO);
        assert_eq!(b.attempts(), 1);
    }

    #[test]
    fn delay_doubles_and_caps() {
        let mut b = RecoveryBackoff::new();
        let seq: Vec<Duration> = (0..8).map(|_| b.next_delay()).collect();
        assert_eq!(
            seq,
            vec![
                Duration::ZERO,
                Duration::from_millis(250),
                Duration::from_millis(500),
                Duration::from_secs(1),
                Duration::from_secs(2),
                Duration::from_secs(4),
                Duration::from_secs(5), // capped, not 8s
                Duration::from_secs(5), // stays capped
            ]
        );
        assert_eq!(b.attempts(), 8);
    }

    #[test]
    fn reset_returns_to_immediate() {
        let mut b = RecoveryBackoff::new();
        b.next_delay();
        b.next_delay();
        assert!(b.attempts() >= 2);
        b.reset();
        assert_eq!(b.next_delay(), Duration::ZERO);
        assert_eq!(b.attempts(), 1);
    }
}
```

- [ ] **Step 2: Run the `RecoveryBackoff` tests to verify they pass**

Run: `cargo test -p pancetta --lib coordinator::audio_recovery -- --nocapture`
Expected: PASS (3 tests) — this is pure arithmetic, should pass on the first real implementation, but
run it anyway per this repo's verification discipline (never assert success without running it).

- [ ] **Step 3: Write the failing tests for `StaleWatchdog`**

Append to the same file, above the existing `#[cfg(test)] mod tests` block's closing brace (i.e.
still inside `mod tests`, alongside the `RecoveryBackoff` tests) — and add the type itself above the
`#[cfg(test)]` block, next to `RecoveryBackoff`:

```rust
/// Edge-detects "just went stale" for the audio relay task's 2-second
/// no-data timeout. Without this, a persistently wedged device would fire a
/// self-triggered `AudioReopenRequest` every single 2s tick forever — each
/// one a real cpal teardown+rebuild, which is wasteful and can itself cause
/// thrashing on a device that's slow to reopen. `on_timeout` returns `true`
/// only on the first tick of a stale episode; `on_data` (called whenever a
/// fresh sample batch arrives) re-arms it for the next episode.
pub struct StaleWatchdog {
    already_signaled: bool,
}

impl StaleWatchdog {
    pub fn new() -> Self {
        Self {
            already_signaled: false,
        }
    }

    /// Call on every stale-timeout tick. Returns `true` exactly once per
    /// stale episode (the transition edge into staleness).
    pub fn on_timeout(&mut self) -> bool {
        if self.already_signaled {
            false
        } else {
            self.already_signaled = true;
            true
        }
    }

    /// Call whenever fresh data arrives, re-arming the watchdog for the next
    /// stale episode.
    pub fn on_data(&mut self) {
        self.already_signaled = false;
    }
}

impl Default for StaleWatchdog {
    fn default() -> Self {
        Self::new()
    }
}
```

Add these tests inside the existing `mod tests` block:

```rust
    #[test]
    fn watchdog_fires_once_per_stale_episode() {
        let mut w = StaleWatchdog::new();
        assert!(w.on_timeout(), "first timeout tick must signal");
        assert!(!w.on_timeout(), "second consecutive tick must not re-signal");
        assert!(!w.on_timeout(), "third consecutive tick must not re-signal");
    }

    #[test]
    fn watchdog_rearms_after_data() {
        let mut w = StaleWatchdog::new();
        assert!(w.on_timeout());
        w.on_data();
        assert!(w.on_timeout(), "must signal again after a fresh data arrival");
    }

    #[test]
    fn watchdog_does_not_fire_before_first_timeout() {
        let w = StaleWatchdog::new();
        // Constructing alone must not have signaled anything — only a real
        // on_timeout() call can. (No API to observe this directly without a
        // call, so this test just documents the invariant via on_timeout's
        // own first-call-returns-true behavior above; kept as a named test
        // so a future refactor that changes the default can't silently flip
        // this without a failing test.)
        let mut w = w;
        assert!(w.on_timeout());
    }
```

- [ ] **Step 4: Run all `audio_recovery` tests to verify they pass**

Run: `cargo test -p pancetta --lib coordinator::audio_recovery -- --nocapture`
Expected: PASS (6 tests total)

- [ ] **Step 5: Register the module**

Modify `pancetta/src/coordinator/mod.rs`, line 19 area:

```rust
mod audio;
mod audio_recovery;
mod autonomous;
```

- [ ] **Step 6: Build the whole workspace to confirm the new module compiles clean**

Run: `cargo build -p pancetta`
Expected: builds with no warnings about the new module (it's currently unused outside its own tests,
so expect a `dead_code` allow may be needed temporarily — do NOT add `#[allow(dead_code)]`; instead
proceed straight to Task 2, which consumes `RecoveryBackoff` immediately. If doing Task 1 as a
standalone commit before Task 2 lands in the same session, it's fine for `cargo build` to warn about
unused pub items in a lib crate — `pancetta` is a binary crate, so unused `pub` items in a private
module DO warn; if the warning appears, that's expected until Task 2 wires it in and will resolve
itself then. Do not suppress it.)

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/coordinator/audio_recovery.rs pancetta/src/coordinator/mod.rs
git commit -m "feat(audio): add RecoveryBackoff + StaleWatchdog pure decision logic

Unused until the next task wires them into the audio thread's recovery
paths; kept in their own module so the backoff/edge-detection policy is
unit-testable without a real AudioManager (docs/audio-robustness-plan.md
items 1 and 4)."
```

---

### Task 2: Wire the auto-recovery supervisor into the audio thread's `Err` arm

**Files:**
- Modify: `pancetta/src/coordinator/audio.rs` (the audio thread's processing loop, currently around
  line 300-330 — **re-grep for `Err(e) => {` inside the `match audio_manager.process_audio()` block
  before editing; this file has churned recently and the exact line number in this plan may be
  stale**)
- Modify: `pancetta-audio/src/manager.rs` (regression test only, see Step 5)

**Interfaces:**
- Consumes: `RecoveryBackoff::new/next_delay/reset/attempts` from Task 1
  (`crate::coordinator::audio_recovery::RecoveryBackoff`).
- Consumes: `AudioManager::reopen_devices(&mut self, input: Option<&str>, output: Option<&str>, force: bool) -> Result<(), AudioError>`
  (existing, `pancetta-audio/src/manager.rs`).
- Consumes: the `report_audio_error` closure already in scope in `start_audio_pipeline`'s audio-thread
  closure (sends `MessageType::Error` to the bus) — reused for one-shot recovery-success logging via a
  new sibling closure, see Step 1.
- Produces (consumed by Task 4, which shares this loop): nothing new exported — this task's changes
  are entirely inside the existing audio-thread closure in `audio.rs`.

- [ ] **Step 1: Read the current code fresh and locate the exact match arms**

Run:
```bash
grep -n "fn start_audio_pipeline\|match audio_manager.process_audio\|Err(e) => {" pancetta/src/coordinator/audio.rs
```

You should find the `loop { ... match audio_manager.process_audio() { Ok(Some(samples)) => ...,
Ok(None) => ..., Err(e) => { ... std::thread::sleep(Duration::from_millis(200)); } } }` block inside
`start_audio_pipeline`'s `std::thread::spawn(move || { ... })` closure (the non-stub branch). Confirm
the closure also has `report_audio_error` (a `String -> ()` closure sending `MessageType::Error`) and
`maybe_report_runtime` (a rate-limited variant) already in scope above the loop — both are reused
below.

- [ ] **Step 2: Add a second diagnostic-reporting closure for retained recovery status**

Immediately after the existing `let mut maybe_report_runtime = |kind: &str, e: String| { ... };`
closure definition (still before the `loop {` line), add:

```rust
                // docs/audio-robustness-plan.md item 1: unlike report_audio_error
                // (an ephemeral MessageType::Error, meant for the TUI's
                // overwrite-prone status line), a "still retrying" recovery
                // state should be RETAINED so the operator can see it wasn't a
                // one-frame blip. Uses the DiagnosticEvent bus (PR #84).
                //
                // Named report_audio_diagnostic (not report_recovery_diagnostic)
                // because Task 4 reuses it for the periodic drop-rate
                // diagnostic — it is a generic level+target+text emitter, not
                // recovery-specific. `target` is a caller-supplied parameter
                // (not hardcoded) precisely so a later, unrelated diagnostic
                // (Task 4's drop-rate report) doesn't get mislabeled under
                // "audio.recovery".
                let report_audio_diagnostic = {
                    let bus = audio_bus.clone();
                    let rt = runtime_handle.clone();
                    move |level: Level, target: &'static str, text: String| {
                        let bus = bus.clone();
                        let diagnostic_level = if level == Level::WARN {
                            pancetta_core::DiagnosticLevel::Warn
                        } else {
                            pancetta_core::DiagnosticLevel::Info
                        };
                        rt.spawn(async move {
                            let msg = ComponentMessage::new(
                                ComponentId::Audio,
                                ComponentId::Tui,
                                MessageType::DiagnosticEvent {
                                    target,
                                    level: diagnostic_level,
                                    text,
                                    qso_id: None,
                                    callsign: None,
                                },
                                Instant::now(),
                            );
                            let _ = bus.send_message(msg).await;
                        });
                    }
                };
                let mut recovery = crate::coordinator::audio_recovery::RecoveryBackoff::new();
                let mut last_recovery_report =
                    std::time::Instant::now() - std::time::Duration::from_secs(60);
                let recovery_report_min_gap = std::time::Duration::from_secs(10);
```

(`Level` is already imported at the top of the file via `use tracing::{error, info, span, Level};` —
reference it as `Level::WARN`/`Level::INFO` in the call sites below, matching the existing import
style.)

- [ ] **Step 3: Replace the log-only `Err` arm with the backoff-driven recovery state machine**

Find the existing arm (verify against current code — this is what it looked like at the time this
plan was written):

```rust
                        Err(e) => {
                            let s = e.to_string();
                            error!("Audio processing error: {}", s);
                            maybe_report_runtime("processing error", s);
                            // docs/audio-robustness-plan.md item 1: a stream
                            // error (e.g. device disconnect) makes
                            // process_audio() return Err on EVERY call from
                            // here on — this arm had no sleep, so this loop
                            // would spin as fast as the CPU allows, forever,
                            // pegging a core and flooding the log with
                            // "Audio processing error" lines (maybe_report_
                            // runtime rate-limits the TUI-facing message, but
                            // not this raw tracing line) until the process is
                            // restarted. Back off like the Ok(None) arm below.
                            std::thread::sleep(std::time::Duration::from_millis(200));
                        }
```

Replace it with:

```rust
                        Err(e) => {
                            let s = e.to_string();
                            error!("Audio processing error: {}", s);
                            maybe_report_runtime("processing error", s);

                            // docs/audio-robustness-plan.md item 1: auto-
                            // recovery. force=true is REQUIRED — with no new
                            // device name, resolve_reopen_targets treats
                            // input=None/output=None as "nothing requested"
                            // and reopen_devices no-ops without force (see
                            // the "Jump Desktop reclaim" case this same
                            // mechanism already handles for the operator
                            // picker). The first attempt after a fresh error
                            // (or after a prior success) fires with zero
                            // delay — a brief USB blip should recover on the
                            // very next loop iteration, not after a sleep.
                            let delay = recovery.next_delay();
                            if !delay.is_zero() {
                                std::thread::sleep(delay);
                            }
                            match audio_manager.reopen_devices(None, None, true) {
                                Ok(()) => {
                                    info!(
                                        "Audio auto-recovery: device reopened \
                                         successfully after {} attempt(s)",
                                        recovery.attempts()
                                    );
                                    if recovery.attempts() > 1 {
                                        report_audio_diagnostic(
                                            Level::INFO,
                                            "audio.recovery",
                                            "Audio device recovered".to_string(),
                                        );
                                    }
                                    recovery.reset();
                                }
                                Err(reopen_err) => {
                                    warn!(
                                        "Audio auto-recovery attempt {} failed: {}",
                                        recovery.attempts(),
                                        reopen_err
                                    );
                                    if last_recovery_report.elapsed() >= recovery_report_min_gap {
                                        report_audio_diagnostic(
                                            Level::WARN,
                                            "audio.recovery",
                                            format!(
                                                "Audio device lost — retrying \
                                                 (attempt {}): {}",
                                                recovery.attempts(),
                                                reopen_err
                                            ),
                                        );
                                        last_recovery_report = std::time::Instant::now();
                                    }
                                }
                            }
                        }
```

Add `warn` to the existing `use tracing::{error, info, span, Level};` import at the top of the file
(becomes `use tracing::{error, info, span, warn, Level};`) — `warn!` is used above and not previously
imported in this file (check first; if already imported, skip this edit).

- [ ] **Step 4: Build and fix any compile errors**

Run: `cargo build -p pancetta`
Expected: clean build. Common issues to check: `Level` import, `recovery`/`last_recovery_report`/
`recovery_report_min_gap`/`report_audio_diagnostic` all declared in the right scope (before the
`loop {`, inside the `std::thread::spawn` closure, alongside `maybe_report_runtime`), and `warn!` now
imported.

- [ ] **Step 5: Add a regression test pinning the `force: true` requirement**

This is the single easiest way to regress this feature (see the plan's "Key findings" item 1) — add
an explicit test to `pancetta-audio/src/manager.rs`'s existing `#[cfg(test)] mod tests` block (find it
near `test_reopen_devices_noop_returns_ok`), asserting the *contract* the recovery loop depends on:

```rust
    #[test]
    fn reopen_unforced_same_device_is_a_true_noop_force_is_required_for_recovery() {
        // Pins the exact contract pancetta/src/coordinator/audio.rs's
        // auto-recovery loop depends on: reopen_devices(None, None, false)
        // on an unchanged device selection is a silent no-op (does NOT
        // rebuild the stream). Auto-recovery has no new device name to
        // supply, so it MUST pass force: true or it silently does nothing
        // while still reporting Ok. This test doesn't assert the stream was
        // rebuilt (no mock cpal layer exists in this crate today) — it pins
        // the resolve_reopen_targets contract the no-op path is built on,
        // which is the part that would silently break this feature.
        assert_eq!(
            resolve_reopen_targets(Some("Rig"), Some("RigOut"), None, None),
            None,
            "no requested change must resolve to no-op regardless of force; \
             force is consulted by reopen_devices AFTER this returns None, \
             which is why auto-recovery must pass force: true explicitly"
        );
    }
```

- [ ] **Step 6: Run the test**

Run: `cargo test -p pancetta-audio --lib reopen_unforced_same_device`
Expected: PASS

- [ ] **Step 7: Run the full existing audio test suites to confirm no regressions**

Run: `cargo test -p pancetta-audio --lib`
Run: `cargo test -p pancetta --lib coordinator`
Expected: all PASS, same pass count as before this task plus the new tests.

- [ ] **Step 8: Commit**

```bash
git add pancetta/src/coordinator/audio.rs pancetta-audio/src/manager.rs
git commit -m "feat(audio): auto-recovery supervisor on process_audio errors

Replaces the log-only Err arm (audio.rs) with a capped-exponential-backoff
reopen loop using the existing reopen_devices primitive. First attempt is
immediate (no sleep) so a brief USB blip recovers on the next loop tick;
repeated failures back off 250ms->5s capped and surface a rate-limited,
RETAINED DiagnosticEvent instead of the prior silent-forever log line.
force=true is required since there's no new device name to supply — pinned
by a new regression test (docs/audio-robustness-plan.md item 1)."
```

---

### Task 3: Silent-death (wedged-device) watchdog via the existing `reopen_tx` channel

**Files:**
- Modify: `pancetta/src/coordinator/audio.rs` (the relay task inside `start_audio_pipeline`, and the
  `reopen_tx`/`reopen_rx` construction just above the audio-thread spawn — **re-grep before editing**)

**Interfaces:**
- Consumes: `StaleWatchdog::new/on_timeout/on_data` from Task 1.
- Consumes: `AudioReopenRequest { input, output, force, respond }` (existing struct, this file,
  defined near the top) and the existing `crossbeam_channel::unbounded::<AudioReopenRequest>()` pair
  (`reopen_tx`/`reopen_rx`) already constructed before the audio thread spawns.
- Produces: nothing new exported — the relay task now additionally *sends* into the existing
  `reopen_rx`, which the audio thread (Task 2's surrounding loop, unmodified by this task) already
  drains every iteration via its existing `while let Ok(req) = reopen_rx.try_recv() { ... }` block.

- [ ] **Step 1: Read the current relay task and channel construction fresh**

Run:
```bash
grep -n "reopen_tx\|reopen_rx\|AUDIO_STALE_TIMEOUT\|health_audio_alive_relay.store(false" pancetta/src/coordinator/audio.rs
```

Confirm: `let (reopen_tx, reopen_rx) = crossbeam_channel::unbounded::<AudioReopenRequest>();` is
constructed *before* `std::thread::spawn` (which moves `reopen_rx` in) and *before* the relay task's
`tokio::spawn` (which does not currently capture `reopen_tx` at all — you're adding that capture).
`self.audio_reopen_tx = Some(reopen_tx);` stores a copy for the TUI picker — `reopen_tx` itself is a
`crossbeam_channel::Sender`, which is `Clone`, so both the stored copy and a clone moved into the
relay task can coexist.

- [ ] **Step 2: Clone `reopen_tx` for the relay task, before it's moved into `self.audio_reopen_tx`**

Find:
```rust
            let (reopen_tx, reopen_rx) = crossbeam_channel::unbounded::<AudioReopenRequest>();
            self.audio_reopen_tx = Some(reopen_tx);
```

Replace with:
```rust
            let (reopen_tx, reopen_rx) = crossbeam_channel::unbounded::<AudioReopenRequest>();
            // docs/audio-robustness-plan.md item 4: the relay task's stale-
            // timeout arm below needs its own sender to self-trigger a
            // recovery reopen through the SAME channel + drain loop the
            // operator's TUI device picker uses — reusing that path means no
            // new cross-thread coordination is needed (the audio thread's
            // loop already serializes every reopen_devices call).
            let reopen_tx_watchdog = reopen_tx.clone();
            self.audio_reopen_tx = Some(reopen_tx);
```

- [ ] **Step 3: Move the cloned sender into the relay task and add the watchdog state**

Find the relay task's `let handle = tokio::spawn(async move { ... });` — confirm `reopen_tx_watchdog`
is captured by the `async move` block (it will be automatically once referenced inside it in the next
step; no separate `let` needed before `tokio::spawn` beyond the clone from Step 2, since the clone
already happened outside the closure and Rust's `move` closure will capture it by value).

Find, near the top of the relay task's body (right after `let mut relay_count: u64 = 0;`):

```rust
                let mut relay_count: u64 = 0;
```

Add immediately after it:

```rust
                let mut relay_count: u64 = 0;
                let mut stale_watchdog = crate::coordinator::audio_recovery::StaleWatchdog::new();
```

- [ ] **Step 4: Wire the watchdog into the timeout arm and the data-received arm**

Find:
```rust
                    let samples =
                        match tokio::time::timeout(AUDIO_STALE_TIMEOUT, result_rx.recv()).await {
                            Ok(Some(samples)) => samples,
                            Ok(None) => break, // sender dropped — audio thread stopped
                            Err(_elapsed) => {
                                health_audio_alive_relay.store(false, Ordering::Relaxed);
                                continue;
                            }
                        };
```

Replace with:
```rust
                    let samples =
                        match tokio::time::timeout(AUDIO_STALE_TIMEOUT, result_rx.recv()).await {
                            Ok(Some(samples)) => {
                                stale_watchdog.on_data();
                                samples
                            }
                            Ok(None) => break, // sender dropped — audio thread stopped
                            Err(_elapsed) => {
                                health_audio_alive_relay.store(false, Ordering::Relaxed);
                                // docs/audio-robustness-plan.md item 4: a
                                // wedged device (callbacks stopped without a
                                // cpal StreamError) sets no flag anywhere
                                // today — process_audio() just keeps
                                // returning Ok(None) forever. Fire exactly
                                // once per stale episode (StaleWatchdog
                                // edge-detects this) rather than every 2s
                                // tick, since each signal is a real cpal
                                // teardown+rebuild on the audio thread.
                                if stale_watchdog.on_timeout() {
                                    warn!(
                                        "Audio watchdog: no samples for {:?} — \
                                         requesting a self-triggered device reopen",
                                        AUDIO_STALE_TIMEOUT
                                    );
                                    let (respond_tx, respond_rx) =
                                        tokio::sync::oneshot::channel();
                                    let sent = reopen_tx_watchdog.send(AudioReopenRequest {
                                        input: None,
                                        output: None,
                                        force: true,
                                        respond: respond_tx,
                                    });
                                    if sent.is_err() {
                                        warn!(
                                            "Audio watchdog: reopen channel closed, \
                                             cannot self-trigger recovery"
                                        );
                                    } else {
                                        tokio::spawn(async move {
                                            match respond_rx.await {
                                                Ok(Ok(())) => info!(
                                                    "Audio watchdog: self-triggered \
                                                     reopen succeeded"
                                                ),
                                                Ok(Err(e)) => warn!(
                                                    "Audio watchdog: self-triggered \
                                                     reopen failed: {}",
                                                    e
                                                ),
                                                Err(_) => {} // audio thread dropped the responder — shutting down
                                            }
                                        });
                                    }
                                }
                                continue;
                            }
                        };
```

`warn` and `info` must be imported at the top of the file — `info` already is; add `warn` if Task 2
didn't already add it to the same `use tracing::{...}` line (check before editing — do not duplicate
the import).

- [ ] **Step 5: Build**

Run: `cargo build -p pancetta`
Expected: clean build. If `AudioReopenRequest` isn't already in scope inside this closure, it's the
same module (`audio.rs`) so no import is needed — confirm the struct definition and this usage are in
the same file.

- [ ] **Step 6: Run the full audio-related test suites**

Run: `cargo test -p pancetta --lib coordinator`
Run: `cargo test -p pancetta-audio --lib`
Expected: all PASS.

- [ ] **Step 7: Commit**

```bash
git add pancetta/src/coordinator/audio.rs
git commit -m "feat(audio): silent-death watchdog self-triggers recovery reopen

The relay task's existing 2s no-data timeout previously only flipped a
reporting-only liveness flag. It now also sends a synthetic, force-flagged
AudioReopenRequest through the SAME channel the TUI device picker uses,
edge-detected via StaleWatchdog so a persistently wedged device gets one
reopen attempt per stale episode rather than one every 2s tick forever.
No new cross-thread coordination needed — the audio thread's existing
single-threaded loop already serializes this against operator-triggered
and Err-arm-triggered reopens (docs/audio-robustness-plan.md item 4)."
```

---

### Task 4: Surface real RX drop-rate as a periodic diagnostic

**Files:**
- Modify: `pancetta-audio/src/manager.rs` (add accessors)
- Modify: `pancetta/src/coordinator/audio.rs` (periodic emission in the audio thread's loop)

**Interfaces:**
- Produces (consumed by this task's own audio.rs change): `AudioManager::dropped_samples(&self) -> u64`,
  `AudioManager::drop_rate_percent(&self) -> f64` (both thin delegations to the existing
  `AudioCommShared::dropped_samples()` / `get_drop_rate()`, `pancetta-audio/src/ringbuffer_comm.rs`,
  already real and already incremented in `push_audio_slice` — do NOT use
  `AudioManagerStats.underruns`/`.overruns`, which are always zero, see the plan's "Key findings"
  item 4).

- [ ] **Step 1: Write the failing test for the new accessors**

Add to `pancetta-audio/src/manager.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn dropped_samples_and_drop_rate_delegate_to_shared_state() {
        if let Ok(manager) = AudioManager::new() {
            // A freshly constructed manager has processed nothing yet.
            assert_eq!(manager.dropped_samples(), 0);
            assert_eq!(manager.drop_rate_percent(), 0.0);
        }
    }
```

- [ ] **Step 2: Run it to verify it fails to compile (methods don't exist yet)**

Run: `cargo test -p pancetta-audio --lib dropped_samples_and_drop_rate`
Expected: FAIL — "no method named `dropped_samples` found"

- [ ] **Step 3: Add the accessors**

In `pancetta-audio/src/manager.rs`, add near `get_stats` (find `pub fn get_stats(&self) -> AudioManagerStats {`):

```rust
    /// Total RX samples dropped by the real-time ring-buffer producer since
    /// the current stream was opened (docs/audio-robustness-plan.md item 4).
    /// Backed by [`AudioCommShared::dropped_samples`], which the RT callback
    /// increments directly — unlike [`AudioManagerStats`]'s `underruns`/
    /// `overruns` fields (never incremented anywhere in this crate), this is
    /// real, live data.
    pub fn dropped_samples(&self) -> u64 {
        self.shared.dropped_samples()
    }

    /// RX sample drop rate as a percentage of (dropped + processed) since the
    /// current stream was opened. See [`dropped_samples`](Self::dropped_samples).
    pub fn drop_rate_percent(&self) -> f64 {
        self.shared.get_drop_rate()
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p pancetta-audio --lib dropped_samples_and_drop_rate`
Expected: PASS

- [ ] **Step 5: Emit a periodic diagnostic from the audio thread's loop**

Re-grep the loop body: `grep -n "let mut recovery = crate::coordinator::audio_recovery" pancetta/src/coordinator/audio.rs`
to find where Task 2's declarations sit (just before `loop {`). Add alongside them:

```rust
                let mut last_drop_report = std::time::Instant::now();
                let drop_report_interval = std::time::Duration::from_secs(30);
```

Inside the `loop { ... }` body, after the `match audio_manager.process_audio() { ... }` block closes
(i.e. as the last statement in each loop iteration, unconditional — runs regardless of which arm of
the match fired), add:

```rust
                    if last_drop_report.elapsed() >= drop_report_interval {
                        last_drop_report = std::time::Instant::now();
                        let dropped = audio_manager.dropped_samples();
                        if dropped > 0 {
                            let rate = audio_manager.drop_rate_percent();
                            let level = if rate > 1.0 { Level::WARN } else { Level::INFO };
                            report_audio_diagnostic(
                                level,
                                "audio.health",
                                format!(
                                    "Audio RX drop rate {:.2}% ({} samples dropped)",
                                    rate, dropped
                                ),
                            );
                        }
                    }
```

This reuses Task 2's `report_audio_diagnostic` closure, which by this point takes a `target:
&'static str` parameter (added as a Task 2 review-fix — see the plan's Task 2 for the current
signature) precisely so this task's diagnostic doesn't get mislabeled under the recovery-specific
`"audio.recovery"` target. Pass `"audio.health"` here since this diagnostic isn't about recovery at
all.

Emit only when `dropped > 0` — per `MessageType::DiagnosticEvent`'s own doc comment ("Emit sparingly
... this is the curated stream, not the tracing firehose"), a healthy stream with zero drops should
emit nothing.

- [ ] **Step 6: Build**

Run: `cargo build -p pancetta`
Expected: clean build.

- [ ] **Step 7: Run the full test suites once more**

Run: `cargo test -p pancetta-audio --lib`
Run: `cargo test -p pancetta --lib coordinator`
Expected: all PASS.

- [ ] **Step 8: Commit**

```bash
git add pancetta-audio/src/manager.rs pancetta/src/coordinator/audio.rs
git commit -m "feat(audio): surface real RX drop-rate as a periodic diagnostic

Adds AudioManager::dropped_samples/drop_rate_percent, backed by the
already-incremented AudioCommShared counters (NOT AudioManagerStats'
underruns/overruns, which are dead/always-zero in this crate). Emitted at
most once per 30s and only when nonzero, via the existing DiagnosticEvent
bus (docs/audio-robustness-plan.md item 4, final piece)."
```

---

### Task 5: Full workspace verification + docs

**Files:**
- Modify: `docs/audio-robustness-plan.md` (status update)
- Modify: `docs/DECISIONS/config-and-platform.md` (append dated entry)

- [ ] **Step 1: Full workspace build and test**

Run: `cargo build --workspace`
Expected: clean build, no warnings introduced by this plan's changes.

Run: `cargo test --workspace --features transmit`
Expected: all pass (per this repo's `CLAUDE.md`, this command is confirmed safe/non-deadlocking).

Run: `cargo clippy --workspace --features transmit -- -D warnings` (or this repo's standard clippy
invocation if different — check for a `check.sh` or CI workflow file first: `ls .github/workflows/`
and `cat check.sh 2>/dev/null` — use whatever the project's actual gate command is instead of
guessing flags).
Expected: clean.

- [ ] **Step 2: Update the design doc's status**

In `docs/audio-robustness-plan.md`, add a status line at the very top (after the title, before
"Detailed plan to..."):

```markdown
**Status: items 1-4 shipped** — 2026-07-11. Auto-recovery supervisor (item 1), missing
`clear_stream_error` (item 2, shipped PR #85 but ultimately unused by the recovery path — see
rationale in the implementation plan), output-side loss detection (item 3, shipped PR #85), and the
silent-death watchdog + drop-rate surfacing (item 4) are all live. Remaining: the real-world
unplug/replug validation test (operator-gated, see `project_meatspace_pending`) and the
operator-switch-vs-recovery coordination risk, which turned out to need no new code — see
"Key findings" in `docs/superpowers/plans/2026-07-11-audio-auto-recovery.md`.
```

- [ ] **Step 3: Append a dated entry to the DECISIONS digest**

Append to `docs/DECISIONS/config-and-platform.md` (end of file):

```markdown

## Audio auto-recovery supervisor (2026-07-11)

Closed `docs/audio-robustness-plan.md`'s remaining design gaps (items 1 and 4 — items 2/3 shipped
earlier in PR #85). The audio thread's `process_audio()` `Err` arm now runs a capped-exponential
backoff (`RecoveryBackoff`, `pancetta/src/coordinator/audio_recovery.rs`: immediate first attempt,
250ms->5s capped thereafter) calling the existing `AudioManager::reopen_devices` primitive with
`force: true` — required because `resolve_reopen_targets` treats an unchanged device selection as a
no-op before `force` is even consulted, so a same-device recovery call with `force: false` would
silently do nothing. A silent-death (wedged-device) watchdog was added to the async relay task's
existing 2-second no-data timeout: instead of only flipping the (already-fixed, PR #85)
`health_audio_alive` reporting flag, it now also sends a synthetic, force-flagged
`AudioReopenRequest` through the SAME `reopen_tx` channel the TUI device picker already uses,
edge-detected via `StaleWatchdog` so a persistently wedged device gets one reopen attempt per stale
episode rather than one every 2s tick. Both new trigger paths ultimately call `reopen_devices` from
inside the audio thread's single-threaded loop, which already serializes every call (operator picker,
Err-arm recovery, watchdog self-trigger) — no new mutex/atomic coordination primitive was needed to
satisfy the design doc's "don't fight the operator's deliberate device switch" risk note. Real RX
drop-rate (`AudioCommShared.dropped_samples`, already incremented in the RT callback — NOT
`AudioManagerStats.underruns`/`.overruns`, confirmed always-zero and dead in this crate) is now
surfaced as a rate-limited `DiagnosticEvent` (at most once per 30s, only when nonzero). Plan:
`docs/superpowers/plans/2026-07-11-audio-auto-recovery.md`. Outstanding: the real unplug/replug
validation is operator-gated hardware time, tracked separately.
```

- [ ] **Step 4: Commit**

```bash
git add docs/audio-robustness-plan.md docs/DECISIONS/config-and-platform.md
git commit -m "docs(audio): mark auto-recovery supervisor shipped, log the decision

docs/audio-robustness-plan.md items 1-4 status update + a dated
config-and-platform.md digest entry per this repo's Documentation Policy."
```

---

## Self-review notes (from writing this plan)

- **Spec coverage:** doc item 1 (auto-recovery) -> Task 2. Item 2 (`clear_stream_error`) -> already
  shipped PR #85; this plan documents (Task 5, "Key findings" item 2) why the recovery path doesn't
  need to call it. Item 3 (output-side detection) -> already shipped PR #85, verified still true, no
  task needed beyond noting it in Task 5's status update. Item 4 (watchdog + counters) -> Tasks 3 and
  4. Doc's "Risks" coordination concern -> resolved by the existing single-threaded loop structure,
  documented rather than coded around (Task 5 status update references this explicitly so it isn't
  silently lost).
- **No placeholders:** every step has literal code, not a description of code.
- **Type consistency check:** `RecoveryBackoff`/`StaleWatchdog` method names and signatures introduced
  in Task 1 are used identically in Tasks 2/3 (`next_delay`/`reset`/`attempts`/`on_timeout`/`on_data`
  — no renames between tasks). `AudioManager::dropped_samples`/`drop_rate_percent` introduced in Task
  4 Step 3 match their use in Task 4 Step 5. Task 2's closure is named `report_audio_diagnostic`
  (not `report_recovery_diagnostic`) precisely so Task 4 Step 5 can reuse it without a rename.
