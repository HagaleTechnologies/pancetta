# Task supervision / crash recovery — design

**Date:** 2026-07-25
**Status:** Approved (operator-reviewed design, this session)
**Status update (2026-07-25):** Phase 1 SHIPPED (this session, `worktree-task-supervision`) — the
coordinator now auto-restarts the 5 components with real no-argument `&mut self -> Result<()>`
start methods (Autonomous, DxCluster, PskReporter, RemoteGateway, Qso), under a 5-restarts/
10-minute rolling budget with capped-exponential backoff, surfacing dropped in-flight QSOs via
the new `QsoFailureReason::SupervisorRestart`. Explicitly deferred as follow-on work: Dsp and
Ft8Decoder need new channel/atomic re-supply plumbing before their moved-once start parameters
can be restarted; Hamlib, StationAgent, and Audio need teardown semantics (safe PTT/TX-arm
unwind) that are only sketched in §2 below, not yet designed in code. See §4.6 for the
already-shipped panic-hook groundwork this builds on.
**Scope crates:** `pancetta` (coordinator), `pancetta-qso` (new `QsoFailureReason` variant)
**Supersedes:** `docs/task-supervision-plan.md` (2026-07-03) — that doc's research and touch
points are correct and are folded in here; this spec is now the authoritative source and
resolves its three open questions.
**Prior art in-repo:** `pancetta/src/coordinator/station_agent/mod.rs` (capped-exponential
reconnect loop, the backoff template); `docs/audio-robustness-plan.md` + PR #121 (a
component-level analog of this same problem, already shipped, informed this design).

## 1. Motivation

There is no runtime supervisor today. Component tasks are spawned, their `JoinHandle`s
collected into one `Vec`, and a 5s poll (`check_task_handles`, `health.rs:412-484`) flips a
status flag to `Failed` exactly once and emits a message. Nothing restarts a dead task; nothing
inspects the panic payload at runtime — `is_finished()` is polled but the finished handle is
never `.await`ed, so clean-return, `Err(..)`, and panic all collapse into the same static
string. For an **unattended** station, a transient panic in the decoder, QSO, or hamlib task
silently kills that capability for the life of the process, with recovery requiring a human
restart.

A process-wide panic hook already shipped (PR #86, `install_panic_hook()` in `main.rs`) —
pure logging/containment, zero recovery behavior. Investigation during that work established
that wrapping the async task bodies in `catch_unwind` doesn't transfer from the synchronous
FT8-decode-loop pattern: `std::panic::catch_unwind` takes a plain `FnOnce() -> R` and cannot
contain `.await` points. Tokio already catches a spawned task's panic at the `JoinHandle`
boundary (that's how `is_finished()` detects a dead task at all) — the panic isn't escaping
unhandled, it's escaping unattended. What's missing isn't panic-catching; it's the supervisor
that acts on the catch tokio already gives for free.

## 2. Goals and non-goals

**Goals**

1. Restart a dead component task automatically, with backoff and a per-component policy, instead
   of leaving the station degraded until a human restart.
2. Make restart safe for safety-critical components (Hamlib PTT, StationAgent TX-arm state) —
   never key TX during a restart window, never leave PTT stuck keyed.
3. Surface every restart (and every restart-caused QSO drop) to the operator, not silently.
4. Contain crash-loops: a component that keeps failing degrades instead of spinning forever.

**Non-goals (explicit)**

- No change to decode/QSO/TX semantics outside of the restart path itself.
- No restart for `Tui` (owns the terminal — DegradeOnly) or for a native `ft8_lib` C abort
  (unrecoverable in-process; the external OS-supervisor, e.g. systemd/launchd
  `Restart=on-failure`, is the documented backstop).
- No cross-component coordination beyond what's specified here (e.g. no attempt to pause other
  components while one restarts).

## 3. Current state (grounded in code)

- **Tracking:** `named_task_handles: Vec<(ComponentId, JoinHandle<Result<()>>)>` (`mod.rs:386`);
  every `start_*_component` pushes one or more handles. 15 `ComponentId`s at implementation time
  (12 originally scoped here plus `Config`, `Coordinator`, and `WsjtxUdp`, discovered when the
  exhaustive `RestartPolicy` match forced their classification — see the implementation plan):
  Audio, Dsp,
  Ft8Decoder, Hamlib, Qso, DxCluster, Ft8Transmitter, Autonomous, PskReporter, RemoteGateway,
  StationAgent, Tui.
- **Detection = polling `is_finished()`, never `.await`.** `check_task_handles` iterates handles
  by reference; on first `is_finished()` it sets `state = Failed(...)`, bumps `error_count`, logs,
  and (Hamlib only) best-effort sends PTT-off — then the dead handle is never looked at again.
- **The channel obstacle:** `create_channel(id)` errors on a duplicate `ComponentId`
  (`message_bus.rs:1065`). Several `start_*_component`s call it in their body, so a naive
  re-invoke after a crash fails on re-registration. This is the single biggest restartability
  blocker.
- **Reusable self-healing patterns:** the station-agent's capped-exponential reconnect loop
  (`station_agent/mod.rs:695-734`, 2s→60s doubling backoff, fails safe with `disarm_on_loss`) is
  the backoff template this design reuses; hamlib's per-call re-open self-heals without task
  restart; the audio auto-recovery supervisor (PR #121) is a component-level analog that proved
  the reopen-on-repeated-error pattern this design generalizes.

## 4. Design

A supervisor lives at the one runtime touchpoint that already holds `&mut self` and the handle
vector: `check_task_handles` (`health.rs:412`), driven by the existing 5s `run_main_loop` tick.

### 4.1 Per-component restart policy

```rust
enum RestartPolicy { Restartable { max_retries: u32, backoff: BackoffSpec }, DegradeOnly, FatalAbort }
```

Extends the existing `component_criticality` seam (`mod.rs:773`) with a third axis. Confirmed
split (operator-reviewed, no changes from the original research):

- **Restartable (cheap):** Ft8Decoder, Dsp, Autonomous, DxCluster, PskReporter, RemoteGateway,
  Qso. Plain restart via the existing `start_*_component` methods, all already `&mut self`.
  Restarting `Qso` drops in-flight QSOs — acceptable for crash recovery, and surfaced per §4.4.
- **Restartable-with-teardown:** Hamlib (tear down untracked child poll/PTT tasks + device handle
  first, then re-connect), StationAgent (re-load identity/keys; **preserve the shared
  `Arc<Mutex<ArmState>>` and its fail-safe disarm** — Part-97 critical), Audio (device re-acquire
  path, same primitive as PR #121's manual-reopen path).
- **DegradeOnly:** Tui (owns the terminal; restart is awkward — degrade + notify).
- **FatalAbort:** native `ft8_lib` C abort — unrecoverable in-process; document the external
  OS-supervisor backstop.

### 4.2 The supervisor loop

On first `is_finished()` for a handle, instead of the terminal `Failed`:

1. **Remove the dead handle and `.await` it** (instant — already finished) to capture the
   outcome: `Ok(Ok(()))` clean, `Ok(Err(e))` component error, `Err(join)` panic/cancel. Requires
   iterating by index / drain, not by reference, since restart mutates the vector under the
   borrow — a real structural change to `check_task_handles`.
2. **Classify + decide** via `RestartPolicy` + the retry budget (§4.3).
3. **Restart:** call the component's `start_*_component` after capped-exponential backoff (lifted
   into a shared `supervised_restart` helper from the station-agent loop). A clean `Ok(())`
   return is **not** an error — a component that exits intentionally (e.g. a disabled
   component's drain task) must not be restarted.
4. **Notify:** reuse the existing `StatusUpdate`/health-summary path (`health.rs:476`) and emit a
   `DiagnosticEvent` — "Ft8Decoder panicked, restart 2/5" is exactly what the operator needs to
   see, riding the bus shipped in item 6 of this session's work.

### 4.3 Retry budget (resolved, was open)

**5 restarts per rolling 10-minute window, per component**, same capped-exponential backoff
shape as the existing station-agent reconnect loop (2s → 60s, doubling). Exceeding the budget
flips the component to `DegradeOnly` — it is not retried again until the process restarts. This
mirrors the existing `ComponentStatus.error_count` field for bookkeeping, with a new rolling-window
timestamp list (or a simple decaying counter) to implement the "per 10 minutes" window.

Single global budget across all `RestartPolicy::Restartable{..}` components (not tiered by
criticality) — simpler to reason about and test; a component that is restarted 5 times in 10
minutes is broken regardless of which tier it's in.

### 4.4 QSO-drop surfacing (resolved, was open)

New `pancetta_qso::QsoFailureReason::SupervisorRestart` variant, alongside the existing
`Timeout`/`SignalLost`/`Duplicate`/`InvalidCallsign`/`FrequencyConflict`/`UserCancelled`/
`Superseded`/`StationQrt`/`ProtocolError`. When the `Qso` component is restarted, every QSO that
was active at the moment of restart is emitted as `QsoEvent::QsoFailed { reason:
SupervisorRestart, .. }` through the existing failure path (the bus emission, the Recent-QSOs
ring, and the color-coded panel shipped this session all pick this up automatically — no new UI
code needed, just the new enum variant and its match arm in `recent_qso_failure_color`).

### 4.5 Channel idempotency

`MessageBus::get_or_create_channel(id)` replaces the duplicate-rejecting `create_channel`
(`message_bus.rs:1065`) — returns the existing registration instead of erroring. Smaller and
safer than having the supervisor tear down channels before restart, and unblocks every
restartable component uniformly. Each `start_*` needs auditing for other first-run-only
assumptions — in particular, Ft8Decoder/Dsp receive a channel receiver that is moved once; the
supervisor needs either a re-creatable channel or a held clone to re-supply on restart.

### 4.6 Panic surface (already shipped, no new work)

`install_panic_hook()` (PR #86) already logs full context (thread/file:line:col/payload)
regardless of TUI/headless mode. No further `catch_unwind` wrapping is being added — see §1 for
why that doesn't transfer to the async task bodies; the supervisor's restart-on-panic *is* the
containment mechanism for these components, not an additional `catch_unwind` layer.

## 5. Touch points (exact hooks)

| Change | File:line |
|---|---|
| Supervisor loop (restart instead of terminal Failed) | `pancetta/src/coordinator/health.rs:412-484` (iterate by index/drain) |
| Restart policy enum + per-component mapping | `pancetta/src/coordinator/mod.rs:764-796` (extend `component_criticality`) |
| Handle registry carries retry/backoff metadata | `mod.rs:386` (value type) or a parallel `HashMap<ComponentId, RestartState>` |
| Restart entry points (already `&mut self`) | the 12 `start_*_component` methods (`mod.rs:1013-1220` + submodules) |
| Channel idempotency | `pancetta/src/message_bus.rs:1065` (`get_or_create_channel`) |
| Backoff helper (lift from station-agent) | `station_agent/mod.rs:695-734` → shared util |
| New QSO failure reason | `pancetta-qso` wherever `QsoFailureReason` is defined |
| Operator notification | `health.rs:476` (`StatusUpdate`) + `DiagnosticEvent` (this session's bus) |

## 6. Testing

- Unit: `RestartPolicy` classification per `ComponentId`.
- Unit: retry-budget/rate-limiter (5 restarts in 10 minutes, 6th attempt within the window →
  `DegradeOnly` instead of another restart).
- Unit: `get_or_create_channel` returns the existing registration without error.
- Integration: spawn a component task that panics on cue → supervisor detects, classifies as
  panic, restarts after backoff, station keeps decoding.
- Integration: a crash-looping task hits the budget and stops (no hot restart-loop).
- Integration: a `Restartable` task that returns `Ok(())` cleanly is **not** restarted.
- Integration: `Qso` restart emits `QsoFailed{reason: SupervisorRestart}` for every in-flight QSO,
  and it appears in the Recent-QSOs panel.
- Safety: StationAgent restart preserves `ArmState` and re-arms only via the normal path; Hamlib
  restart tears down child tasks and leaves PTT off.

## 7. Risks / rollout

- **Crash-loop guard is mandatory** — covered by §4.3.
- **Qso restart loses in-flight QSOs** — acceptable for crash recovery, surfaced per §4.4.
- **Safety-critical components must fail safe across a restart** — PTT off, disarmed, never key
  TX during a restart window. This is why teardown-restart for Hamlib/StationAgent/Audio ships
  last, after the cheap-restartable set has proven the supervisor mechanics.
- **Ship order:** (1) panic hook — already shipped; (2) `get_or_create_channel`; (3) supervisor
  for the cheap-restartable set; (4) teardown-restart for Hamlib/StationAgent/Audio last, with
  their safety invariants. Additive; the default `RestartPolicy` for anything unclassified stays
  `DegradeOnly` (today's behavior), so the change can't regress a component into an unsafe
  restart.
- Keep the external OS-supervisor (systemd/launchd) as the backstop for `FatalAbort`; document it.
