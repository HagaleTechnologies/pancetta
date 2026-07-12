# Audio device robustness — implementation plan

**Status: items 1-4 shipped** — 2026-07-11. Auto-recovery supervisor (item 1), missing
`clear_stream_error` (item 2, shipped PR #85 but ultimately unused by the recovery path — see
rationale in the implementation plan), output-side loss detection (item 3, shipped PR #85), and the
silent-death watchdog + drop-rate surfacing (item 4) are all live. Remaining: the real-world
unplug/replug validation test (operator-gated, see `project_meatspace_pending`) and the
operator-switch-vs-recovery coordination risk, which turned out to need no new code — see
"Key findings" in `docs/superpowers/plans/2026-07-11-audio-auto-recovery.md`.

Detailed plan to make the audio path survive a **disconnected, replugged, or wedged** USB
codec — auto-recovering instead of going silently dead until a manual restart. **Plan only
— no code here.** Grounded in the current code.

## Problem

Audio is the RX/TX lifeblood; a wedged or unplugged USB codec kills the station. The
*detection* primitive for input-device loss exists and init-failure surfacing is solid,
but there is **no recovery**: no auto-reinit, no error-flag reset, no replug re-acquire, no
output-side loss detection, no wedged-device (silent-death) watchdog — plus a CPU
hot-spin bug once the error latches.

## Current state

- **Architecture:** dedicated audio thread → `AudioManager` → `AudioStreamManager` → cpal
  streams, bridged by lock-free `HeapRb<f32>` ring buffers. RX ring 8192 f32 (~170 ms @
  48 kHz); TX ring 786_432 f32 (~16 s, holds a full 12.64 s TX). Streams run at 48 kHz;
  RX is *not* resampled in-manager (DSP does 48k→12k). TX output forced mono, silence-fills
  on underrun.
- **Device selection + init-failure surfacing (solid):** CLI `--audio-device` or config
  name; substring match with fall-back-to-best on no-match. Init failures surface to the
  TUI as `MessageType::Error` via `report_audio_error` (`coordinator/audio.rs:166-185`):
  manager-construction failure, stream-start failure, and the "TX audio on system default
  → PTT keys rig but audio goes to speakers" warning. Runtime errors rate-limited 1/5 s.
- **Over/underrun:** RX overflow (decode falls behind) → samples **silently dropped** into
  a `dropped_samples` counter inside the RT callback (no log at drop time). TX underrun →
  silence (normal). TX overrun (queue full) → a `warn!`, tail dropped. **`StreamStatistics`
  / `AudioManagerStats` are computed but never consumed** — the running station surfaces
  none of it.

### The gaps (all real)

1. **No auto-reconnect.** On input `StreamError`, cpal's error callback sets a shared flag
   (`stream.rs:358/373/388`); `process_audio` reads it and returns `Err` **forever**
   (`manager.rs:245`). The coordinator's `Err` arm (`audio.rs:311`) **only logs** — it never
   tears down + rebuilds. The reinit primitive (`reopen_devices` /
   `apply_devices_to_stream`, `manager.rs:418-528`) **exists but is only operator-triggered**
   via the TUI device picker.
2. **Error flag never cleared** — `set_stream_error` only stores `true`; there's no
   `clear_stream_error` (`ringbuffer_comm.rs:62`). In-place recovery is impossible without
   building a whole new stream manager.
3. **CPU hot-spin.** Once the flag latches, `process_audio` returns `Err` every iteration
   and the coordinator `Err` arm **doesn't sleep** (only `Ok(None)` sleeps 1 ms,
   `audio.rs:308`) → the audio thread busy-spins ~100%.
4. **Output loss undetected.** The output error callback (`stream.rs:568`) only `eprintln!`s
   — it does **not** call `set_stream_error()`. Unplug mid-*transmission* and TX audio goes
   nowhere with no signal (the input flag isn't shared to the output side).
5. **Replug not auto-acquired.** A cpal stream bound to a removed device does not re-attach;
   recovery needs `refresh_devices()` + a fresh `build_*_stream` — operator-only today.
6. **Silent-death / wedged device undetected.** A device that stops delivering callbacks
   *without* a `StreamError` sets no flag. `health_audio_alive` is **write-once-true** (only
   `store(true)` exists, `audio.rs:370`; grep-confirmed no `store(false)`), so the TUI
   "audio alive" badge latches alive permanently. `last_audio_timestamp` *does* carry a real
   last-sample epoch (`audio.rs:335`, read at `health.rs:493`) but is **log-only** — never
   thresholded into a watchdog.

## Design

Four pieces; the reinit engine (`reopen_devices`) already exists — most of this is wiring
an **auto-recovery supervisor** into the audio thread plus closing the detection gaps.

### 1. Auto-recovery supervisor in the audio loop

Replace the log-only `Err` arm (`audio.rs:311`) with a recovery state machine:
- On `process_audio` → `Err(stream error)`: **back off** (sleep — fixes the hot-spin, item
  3), then attempt `reopen_devices` (the existing primitive) with **capped-exponential
  backoff** (reuse the shared backoff helper from the supervision plan). On success, clear
  the error and resume; on repeated failure, keep retrying at the capped interval and
  surface a persistent "audio device lost — retrying" status (not a one-frame blip).
- **Cross-reference:** this is a component-level analog of the task-supervision plan; the
  audio *thread* stays alive and re-acquires the *device* rather than being killed +
  restarted. Prefer in-thread device re-acquire (cheaper, preserves the thread + channels)
  over full task restart.

### 2. Add the missing `clear_stream_error`

`AudioCommShared` needs `clear_stream_error()` (`ringbuffer_comm.rs:62`) so recovery can
reset the latch on the existing shared struct — or, if `reopen_devices` always builds a
fresh `shared`, ensure the recovery path swaps in the new manager atomically and the audio
loop reads the new flag. Pick one and make the ownership explicit.

### 3. Output-side loss detection

Share the stream-error flag (or a second output flag) into the **output** error callback
(`stream.rs:568`) so an unplug mid-transmission sets it, feeding the same recovery path.
Today only the input side propagates. This closes item 4 (silent TX-into-the-void).

### 4. Silent-death (wedged-device) watchdog + honest liveness

- **Watchdog:** threshold `last_audio_timestamp` (`audio.rs:335`) — if no RX sample has
  arrived for > T (e.g. a few 100 ms of expected 48 kHz flow), treat the device as wedged:
  set the error flag (triggering recovery in item 1) and surface it. This catches the
  "callbacks stopped without a StreamError" case that nothing detects today.
- **Fix the liveness latch:** make `health_audio_alive` reflect reality — `store(false)`
  when the watchdog trips / stream errors, `store(true)` on recovery — so the TUI badge
  (`tui_relay.rs:538`) stops lying. This is also the piece deferred in
  `project_ssh_tmux_pending` ("audio-init-visibility shipped, latched-relay fix deferred
  pending a restart story") — the recovery supervisor **is** that restart story, so this
  fix can now land with it.
- **Surface the counters:** feed the already-computed `StreamStatistics.dropped_samples` /
  underrun counts into the health/diagnostics surface (observability plan) so RX-overflow
  ("decode falling behind") and device health are visible instead of hidden in an
  unconsumed struct.

## Touch points (exact hooks)

| Change | File:line |
|---|---|
| Auto-recovery state machine (backoff + reopen) replacing log-only Err arm | `pancetta/src/coordinator/audio.rs:311-315` |
| Reinit primitive (exists, reuse) | `pancetta-audio/src/manager.rs:418-528` (`reopen_devices`/`apply_devices_to_stream`), `device.rs:88` (`refresh_devices`) |
| `clear_stream_error` (missing) | `pancetta-audio/src/ringbuffer_comm.rs:62-69` |
| Output-side loss propagation (missing) | `pancetta-audio/src/stream.rs:568-571` (add `set_stream_error`; share flag to output) |
| Silent-death watchdog | threshold `last_audio_timestamp` (`audio.rs:335`, read `health.rs:493`) |
| Honest liveness (stop write-once-true) | `audio.rs:370` (add `store(false)` path), badge at `tui_relay.rs:538` |
| Surface drop/underrun counters | `stream.rs:245` (`StreamStatistics`), `manager.rs:303` (`AudioManagerStats`) → health/diagnostics |
| Backoff helper | shared util (also used by task-supervision + station-agent) |

## Tests

- Unit: `clear_stream_error` resets the latch; `set` then `clear` round-trip.
- Unit: watchdog fires when `last_audio_timestamp` is older than T; does not fire under
  normal flow.
- Unit: `health_audio_alive` goes false on error/watchdog and true on recovery (regression
  guard against the write-once-true bug).
- Integration (mockable device layer): simulate an input `StreamError` → recovery loop
  attempts `reopen_devices` with backoff, does **not** hot-spin (assert bounded CPU / sleep
  present), recovers on the next successful reopen.
- Integration: output error callback sets the flag (simulated mid-TX device loss).
- Manual/operator: the real **unplug → replug the USB codec** path end-to-end (this is the
  outstanding real-world test in `project_ssh_tmux_pending`); auto-recovery should re-acquire
  without an app restart.

## Risks / rollout

- **Don't fight the operator's deliberate device switch** — the recovery path and the
  existing `AudioReopenRequest` picker must coordinate (recovery backs off while an operator
  switch is in flight).
- **Backoff is mandatory** to avoid hammering a permanently-absent device (and to fix the
  hot-spin).
- **Replug may present a new device handle/name** — recovery must `refresh_devices()` and
  re-match by name, tolerating the handle change (the codebase already re-enumerates output
  devices per-call for exactly this bidirectional-USB reason, `stream.rs:413`).
- **TX safety:** on output-device loss mid-transmission, surface it and let the QSO/TX layer
  know audio isn't reaching the rig (a keyed PTT with no audio is a dead carrier) — coordinate
  with the drop-stale-TX / TX-policy path so we don't sit keyed into a dead device.
- **Ship order:** (1) fix the hot-spin (backoff on the Err arm) + `clear_stream_error` — pure
  bug fixes; (2) output-side detection + honest liveness; (3) the full auto-reopen supervisor;
  (4) the silent-death watchdog. Additive; default behavior without the supervisor is today's
  (log + degrade), so each piece is independently safe. Depends on nothing rig-side; the only
  true validation is the physical unplug/replug test.
