# Task supervision / crash recovery — implementation plan

Detailed plan to make the coordinator **restart a dead component task** (with backoff and
policy) instead of leaving the station degraded until a human restarts the whole app.
**Plan only — no code here.** Grounded in the current code.

## Problem

There is **no runtime supervisor.** Component tasks are spawned, their `JoinHandle`s
collected into one `Vec`, and a 5s poll flips a status flag to `Failed` exactly once and
emits a message. Nothing restarts a dead task; nothing even inspects the panic payload at
runtime. `mod.rs:788` — "FT8 decoder crashed -- no decoding until restart" — is an
accurate description of the only recovery available today: a human restart. For an
**unattended** station, a transient panic in the QSO, hamlib, or decode task silently
kills that capability for the life of the process.

## Current state

- **Tracking:** `named_task_handles: Vec<(ComponentId, JoinHandle<Result<()>>)>`
  (`mod.rs:386`); every `start_*_component` pushes one+ handles. `ComponentId`
  (`message_bus.rs:33`): Audio, Dsp, Ft8Decoder, Hamlib, Qso, DxCluster, Ft8Transmitter,
  Autonomous, PskReporter, RemoteGateway, StationAgent, Tui.
- **Detection = polling `is_finished()`, never `.await`.** `check_task_handles`
  (`health.rs:412-484`) iterates handles **by reference**, and on first `is_finished()`
  sets `state = Failed(degradation_message(id))`, `error_count += 1`, logs, and — for
  Hamlib only — best-effort sends PTT-off. Then `if state != Running { continue }` — the
  dead handle is **never looked at again**. Because it never `.await`s the finished
  handle, it **cannot distinguish clean-return vs `Err(..)` vs panic** — all collapse to
  the same static string.
- **Panic behavior:** `panic = "unwind"` (deliberate, so the FT8 loop can `catch_unwind`);
  **no `set_panic_hook` anywhere**, so a panic unwinds only its task/thread (process
  survives). The FT8 hot loop is panic-guarded **only around the two decode calls**
  (`ft8.rs:497,558`, bumping `DECODE_PANIC_COUNT`); a panic elsewhere in that loop, or in
  the **qso/hamlib/audio/autonomous** tasks (which have **no `catch_unwind`**), kills the
  task → marked Failed → dead forever. A native `abort` inside ft8_lib C cannot unwind and
  would take the process down (the documented OS-supervisor backstop).
- **The channel obstacle:** `create_channel(id)` **errors on a duplicate ComponentId**
  (`message_bus.rs:1065`). Several `start_*_component`s call `create_channel` in their
  body, so a naïve re-invoke after a crash fails on re-registration. **This is the single
  biggest restartability blocker.**
- **Existing self-healing patterns (reusable):** the station-agent **capped-exponential
  reconnect loop** (`station_agent/mod.rs:695-734`, `backoff = (backoff*2).min(MAX)`,
  fails safe with `disarm_on_loss`) is the cleanest template; hamlib's per-call re-open +
  `consecutive_failures`/`PollingFailed` badge self-heals without task restart; the audio
  `AudioSwitchRequest` is a deliberate device re-init path.

## Design

A supervisor that lives at the one runtime touchpoint that already holds `&mut self` and
the handle vector: **`check_task_handles` (`health.rs:412`)**, driven by the existing 5s
`run_main_loop` tick.

### 1. Per-component restart policy

Extend the existing `component_criticality` seam (`mod.rs:773`) with a third axis:

```
enum RestartPolicy { Restartable{max_retries, backoff}, DegradeOnly, FatalAbort }
```

Recommended split (from restartability analysis):
- **Restartable (cheap):** Ft8Decoder, Dsp, Autonomous, DxCluster, PskReporter,
  RemoteGateway, Qso *(restart drops in-flight QSOs — acceptable for crash recovery; must
  be understood + surfaced)*.
- **Restartable-with-teardown:** Hamlib (tear down untracked child poll/PTT tasks + device
  handle, then re-connect), StationAgent (re-load identity/keys; **preserve the shared
  `Arc<Mutex<ArmState>>` and its fail-safe disarm** — Part-97), Audio (needs the device
  re-acquire path, see the audio-robustness plan).
- **DegradeOnly:** Tui (owns the terminal; restart is awkward — degrade + notify).
- **FatalAbort:** a native ft8_lib C abort — unrecoverable in-process; document the
  external OS-supervisor (systemd/launchd `Restart=on-failure`) as the backstop.

### 2. The supervisor loop (in `check_task_handles`)

On first `is_finished()` for a handle, instead of the terminal `Failed`:
1. **Remove the dead handle and `.await` it** (it's already finished → instant) to capture
   the outcome: `Ok(Ok(()))` clean, `Ok(Err(e))` component error, `Err(join)` panic/cancel.
   This requires iterating **by index / drain**, not `&self.named_task_handles`
   (`health.rs:413`) — a real structural change, since restart mutates the vector under the
   borrow.
2. **Classify + decide** via `RestartPolicy` + a per-component retry budget (reuse
   `ComponentStatus.error_count`, `mod.rs:747`) + a restart-rate limiter (e.g. ≤N restarts
   per rolling window, else give up → DegradeOnly to avoid crash-loops).
3. **Restart:** call the component's `start_*_component` (all already `&mut self`, all
   re-push a fresh handle) after a **capped-exponential backoff** (lift the station-agent
   loop into a shared `supervised_restart` helper). A clean `Ok(())` return is **not** an
   error — don't restart a task that exited intentionally (e.g. a disabled component's
   drain task).
4. **Notify:** reuse the existing `StatusUpdate`/health-summary path (`health.rs:476`);
   also emit a `DiagnosticEvent` (see observability plan) — "Ft8Decoder panicked, restart
   2/5" is exactly what the operator needs to see.

### 3. Make `start_*_component` idempotent w.r.t. channels

The channel-duplicate blocker: add a `MessageBus::get_or_create_channel(id)` (return the
existing registration instead of erroring), **or** have the supervisor tear down the
component's channel before restart. `get_or_create_channel` is the smaller, safer change
and unblocks every restartable component uniformly. Audit each `start_*` for other
first-run assumptions (moved crossbeam receivers — Ft8Decoder/Dsp receive a `ft8_rx`/DSP RX
that is **moved once**; the supervisor must either keep a re-creatable channel or hold a
clone to re-supply).

### 4. Narrow the panic surface (defense in depth)

Independently of restart: widen `catch_unwind` coverage or wrap the qso/hamlib/autonomous
task bodies so a transient panic is *contained + logged + counted* rather than task-fatal —
mirroring the decode-loop guard. Add a process-level `set_panic_hook` that logs the panic
(location + payload) to the file log and emits a diagnostic, so panics are never silent.

## Touch points (exact hooks)

| Change | File:line |
|---|---|
| Supervisor loop (restart instead of terminal Failed) | `pancetta/src/coordinator/health.rs:412-484` (iterate by index/drain) |
| Restart policy enum + per-component mapping | `pancetta/src/coordinator/mod.rs:764-796` (extend `component_criticality`) |
| Handle registry carries retry/backoff metadata | `mod.rs:386` (value type) or a parallel `HashMap<ComponentId, RestartState>` |
| Restart entry points (already `&mut self`) | the 12 `start_*_component` methods (`mod.rs:1013-1220` + submodules) |
| Channel idempotency | `pancetta/src/message_bus.rs:1065` (`get_or_create_channel`) |
| Backoff helper (lift from station-agent) | `station_agent/mod.rs:695-734` → shared util |
| Panic hook + wider `catch_unwind` | `main.rs` (hook), `coordinator/{qso,hamlib,autonomous}.rs` task bodies |
| Operator notification | `health.rs:476` (`StatusUpdate`) + `DiagnosticEvent` |

## Tests

- Unit: `RestartPolicy` classification per `ComponentId`; retry-budget + rate-limiter
  (N restarts then give up → DegradeOnly).
- Unit: `get_or_create_channel` returns existing registration without error (vs today's
  duplicate-reject at `message_bus.rs:1065`).
- Integration: spawn a component task that panics on cue → supervisor detects, classifies
  as panic, restarts after backoff, station keeps decoding; a crash-looping task hits the
  budget and stops (no hot restart-loop).
- Integration: a Restartable task that returns `Ok(())` cleanly is **not** restarted.
- Safety: StationAgent restart preserves `ArmState` and re-arms only via the normal path;
  Hamlib restart tears down child tasks + leaves PTT off.

## Risks / rollout

- **Crash-loop guard is mandatory** — an always-panicking component must degrade after N
  fast restarts, not spin. Rate-limit + max-retries.
- **Qso restart loses in-flight QSOs** — acceptable for crash recovery, but surface it
  ("QSO engine restarted, N active QSOs dropped").
- **Safety-critical components** (Hamlib PTT, StationAgent arm) must fail *safe* across a
  restart (PTT off, disarmed) — never key TX during a restart window.
- **Ship order:** (1) panic hook + wider `catch_unwind` (pure containment, no restart
  risk); (2) `get_or_create_channel`; (3) supervisor for the cheap-restartable set
  (Ft8Decoder/Dsp/Autonomous/network clients); (4) teardown-restart for
  Hamlib/StationAgent/Audio last, with their safety invariants. Additive; the default
  `RestartPolicy` for anything unclassified stays **DegradeOnly** (today's behavior), so
  the change can't regress a component into an unsafe restart.
- Keep the **external OS-supervisor** (systemd/launchd) as the backstop for FatalAbort;
  document it.
