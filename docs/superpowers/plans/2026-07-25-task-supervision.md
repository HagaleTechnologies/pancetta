# Task Supervision (Phase 1: cheap-restartable set) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the coordinator automatically restart a dead component task (with backoff, a
retry budget, and operator notification) for the five no-argument cheap-restartable components
(Autonomous, DxCluster, PskReporter, RemoteGateway, Qso), instead of leaving the station
degraded until a human restarts the whole app.

**Architecture:** Restructure `check_task_handles` (the existing 5s health-poll) to drain and
`.await` finished handles (classifying clean-exit vs. error vs. panic), consult a new
`RestartPolicy` per `ComponentId`, and either restart (capped-exponential backoff, rate-limited)
or degrade. Channel re-registration is unblocked via a new `get_or_create_channel`. Qso restarts
additionally surface dropped in-flight QSOs through the existing Recent-QSOs panel via a new
`QsoFailureReason::SupervisorRestart` variant.

**Tech Stack:** Rust workspace (existing), tokio (task/channel primitives already in use),
existing `MessageBus`/`DiagnosticEvent` bus (PR #84 + this session's work).

**Spec:** `docs/superpowers/specs/2026-07-25-task-supervision-design.md` — read it first.

## Global Constraints

- **Scope of this plan:** only the 5 no-argument components (`start_autonomous_component`,
  `start_dx_cluster_component`, `start_pskreporter_component`,
  `start_remote_gateway_component`, `start_qso_component` — all `pub(crate) async fn(&mut self)
  -> Result<()>`, verified against current source). **Dsp and Ft8Decoder are explicitly OUT of
  scope** — their start functions (`start_dsp_pipeline`, `start_ft8_pipeline`) take
  crossbeam-channel/atomic parameters that were moved-once at boot and need a re-suppliable-clone
  design before they're restartable; that's a follow-on plan. **Hamlib, StationAgent, Audio
  (the teardown-restart tier) and Tui/FatalAbort are also out of scope** — per the spec's §7 ship
  order, they ship last, after this mechanism proves out. `RestartPolicy` classification (Task 2)
  covers all 12 `ComponentId`s regardless, so the safety net (`DegradeOnly` default) is in place
  from Task 2 onward even before Dsp/Ft8Decoder/Hamlib/StationAgent/Audio get real restart
  dispatch.
- **A clean `Ok(())` return must NOT trigger a restart** — some components exit intentionally.
- **Retry budget:** 5 restarts per rolling 10-minute window per component, capped-exponential
  backoff (2s → 60s, doubling) — same shape as `station_agent/mod.rs`'s existing
  `RECONNECT_BACKOFF_MIN`/`RECONNECT_BACKOFF_MAX`.
- **Never restart a task while iterating `named_task_handles` by reference** — the restructure to
  drain/index-based iteration is required because restart mutates the vector.
- **Subagent rules (standing):** implementers never push / never destructive git; controller
  pushes at batch boundaries. Local `cargo fmt` + `cargo clippy` before each commit.
- **Don't touch** the already-shipped panic hook (`install_panic_hook()` in `main.rs`) or the
  Hamlib PTT-off-on-failure logic in `check_task_handles` (`health.rs:491-501`) — both stay as-is.

---

## Task 1: `MessageBus::get_or_create_channel`

**Files:**
- Modify: `pancetta/src/message_bus.rs` (add method near `create_channel`, currently at line 1076)
- Test: same file's test module (`#[cfg(test)] mod tests`)

**Interfaces:**
- Produces: `pub async fn get_or_create_channel(&self, component_id: ComponentId) -> Result<(Sender<ComponentMessage>, Receiver<ComponentMessage>)>` — returns the existing registration's sender/receiver clones if one exists, otherwise behaves exactly like `create_channel`.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn get_or_create_channel_returns_existing_registration_without_error() {
    let bus = MessageBus::new(MessageBusConfig::default());
    let (tx1, _rx1) = bus.create_channel(ComponentId::Dsp).await.unwrap();
    let (tx2, _rx2) = bus.get_or_create_channel(ComponentId::Dsp).await.unwrap();
    // Same underlying channel: a message sent via tx1 must be visible to a
    // receiver obtained from the second call's rx (same crossbeam sender clone).
    tx1.send(ComponentMessage::new(
        ComponentId::Coordinator,
        ComponentId::Dsp,
        MessageType::Shutdown,
        std::time::Instant::now(),
    ))
    .unwrap();
    drop(tx2); // tx2 is a clone of the same sender; dropping it must not close the channel
    // A fresh create_channel for the SAME id must still error (proves get_or_create
    // didn't silently replace the registration under the hood).
    assert!(bus.create_channel(ComponentId::Dsp).await.is_err());
}
```

(Adjust `MessageBusConfig::default()`/`ComponentMessage::new` construction to match whatever the
existing tests in this file use — copy the pattern from a neighboring test rather than guessing.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta get_or_create_channel_returns_existing --lib -- --nocapture`
Expected: FAIL with "no method named `get_or_create_channel`"

- [ ] **Step 3: Implement**

Read `create_channel` (`message_bus.rs:1076-1096` at plan time — re-verify) fully first. Add:

```rust
/// Idempotent variant of `create_channel`: returns the existing registration's
/// sender/receiver if `component_id` is already registered, instead of erroring.
/// Needed so a supervisor-restarted component's `start_*_component` (which calls
/// `create_channel` internally) can be re-invoked without a special restart-only
/// code path in every component.
pub async fn get_or_create_channel(
    &self,
    component_id: ComponentId,
) -> Result<(Sender<ComponentMessage>, Receiver<ComponentMessage>)> {
    let channels = self.channels.read().await;
    if let Some(existing) = channels.get(&component_id) {
        return Ok((existing.sender.clone(), existing.receiver.clone()));
    }
    drop(channels);
    self.create_channel(component_id).await
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta get_or_create_channel_returns_existing --lib`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/message_bus.rs
git commit -m "feat(coordinator): MessageBus::get_or_create_channel for restart idempotency"
```

---

## Task 2: `RestartPolicy` enum + full `ComponentId` classification

**Files:**
- Modify: `pancetta/src/coordinator/mod.rs` (near `ComponentCriticality`, currently line 1098)
- Test: same file's test module

**Interfaces:**
- Produces:

```rust
/// How the supervisor (Task 5) should react when a component's task dies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RestartPolicy {
    /// Re-invoke the component's `start_*` method after backoff, up to the
    /// shared retry budget (Task 3).
    Restartable,
    /// Never auto-restart; log and leave `Failed` (today's behavior).
    DegradeOnly,
    /// Unrecoverable in-process (e.g. a native C abort); document the
    /// external OS-supervisor as the backstop. Treated identically to
    /// `DegradeOnly` by the supervisor loop — the distinction is purely
    /// for the log message and for future extension, not behavior.
    FatalAbort,
}

/// Per-component restart policy (spec: docs/superpowers/specs/2026-07-25-task-supervision-design.md §4.1).
/// Default for anything not listed is `DegradeOnly` (today's behavior) —
/// this is the safety net: a component only gets auto-restarted if it's
/// explicitly classified `Restartable` here.
pub(crate) fn component_restart_policy(id: ComponentId) -> RestartPolicy {
    match id {
        ComponentId::Autonomous
        | ComponentId::DxCluster
        | ComponentId::PskReporter
        | ComponentId::RemoteGateway
        | ComponentId::Qso => RestartPolicy::Restartable,
        // Out of scope for this plan (need channel/atomic re-supply design):
        ComponentId::Dsp | ComponentId::Ft8Decoder => RestartPolicy::DegradeOnly,
        // Out of scope for this plan (need teardown semantics):
        ComponentId::Hamlib | ComponentId::StationAgent | ComponentId::Audio => {
            RestartPolicy::DegradeOnly
        }
        // Owns the terminal; restart is awkward — degrade + notify.
        ComponentId::Tui => RestartPolicy::DegradeOnly,
        // Never a real task-handle target for this supervisor.
        ComponentId::Config | ComponentId::Coordinator | ComponentId::Ft8Transmitter => {
            RestartPolicy::DegradeOnly
        }
    }
}
```

Check the real `ComponentId` variant list (`message_bus.rs:33-70` at plan time) before writing
this match — the plan's list above was verified against current source but re-verify at
implementation time in case a variant was added/removed since. The match must be exhaustive
(compiler-enforced) — this is deliberate: adding a new `ComponentId` forces a decision here.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn restart_policy_matches_spec_classification() {
    use RestartPolicy::*;
    assert_eq!(component_restart_policy(ComponentId::Qso), Restartable);
    assert_eq!(component_restart_policy(ComponentId::Autonomous), Restartable);
    assert_eq!(component_restart_policy(ComponentId::DxCluster), Restartable);
    assert_eq!(component_restart_policy(ComponentId::PskReporter), Restartable);
    assert_eq!(component_restart_policy(ComponentId::RemoteGateway), Restartable);
    assert_eq!(component_restart_policy(ComponentId::Dsp), DegradeOnly);
    assert_eq!(component_restart_policy(ComponentId::Ft8Decoder), DegradeOnly);
    assert_eq!(component_restart_policy(ComponentId::Hamlib), DegradeOnly);
    assert_eq!(component_restart_policy(ComponentId::StationAgent), DegradeOnly);
    assert_eq!(component_restart_policy(ComponentId::Audio), DegradeOnly);
    assert_eq!(component_restart_policy(ComponentId::Tui), DegradeOnly);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta restart_policy_matches_spec --lib`
Expected: FAIL with "cannot find function `component_restart_policy`"

- [ ] **Step 3: Implement** (the enum + function above)

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta restart_policy_matches_spec --lib`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/mod.rs
git commit -m "feat(coordinator): RestartPolicy enum + per-component classification"
```

---

## Task 3: Retry budget + shared backoff helper

**Files:**
- Create: `pancetta/src/coordinator/restart_budget.rs`
- Modify: `pancetta/src/coordinator/mod.rs` (register the module: `mod restart_budget;`)
- Test: `restart_budget.rs`'s own `#[cfg(test)] mod tests`

**Interfaces:**
- Produces (consumed by Task 5):

```rust
// pancetta/src/coordinator/restart_budget.rs
use std::collections::HashMap;
use std::time::{Duration, Instant};

use crate::message_bus::ComponentId;

/// Reconnect/restart backoff (capped) — same shape as
/// `station_agent/mod.rs`'s `RECONNECT_BACKOFF_MIN`/`MAX`.
pub(crate) const RESTART_BACKOFF_MIN: Duration = Duration::from_secs(2);
pub(crate) const RESTART_BACKOFF_MAX: Duration = Duration::from_secs(60);

/// How many restarts a component gets before the supervisor gives up and
/// leaves it `DegradeOnly` (spec §4.3): 5 restarts per rolling 10-minute
/// window.
const MAX_RESTARTS_PER_WINDOW: usize = 5;
const RESTART_WINDOW: Duration = Duration::from_secs(600);

/// Per-component restart bookkeeping: restart timestamps (for the rolling
/// window) and the current backoff duration (reset to `RESTART_BACKOFF_MIN`
/// whenever the window has no restarts in it).
#[derive(Debug, Default)]
pub(crate) struct RestartBudget {
    attempts: HashMap<ComponentId, Vec<Instant>>,
    backoff: HashMap<ComponentId, Duration>,
}

impl RestartBudget {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Prune attempts older than `RESTART_WINDOW`, then report whether
    /// another restart is allowed right now for `id`.
    pub(crate) fn may_restart(&mut self, id: ComponentId, now: Instant) -> bool {
        let attempts = self.attempts.entry(id).or_default();
        attempts.retain(|&t| now.duration_since(t) < RESTART_WINDOW);
        attempts.len() < MAX_RESTARTS_PER_WINDOW
    }

    /// Record a restart attempt at `now` and return the backoff to sleep
    /// before actually restarting (doubles each call within the window,
    /// resets to `RESTART_BACKOFF_MIN` once the window has aged out).
    pub(crate) fn record_attempt_and_backoff(&mut self, id: ComponentId, now: Instant) -> Duration {
        let attempts = self.attempts.entry(id).or_default();
        attempts.retain(|&t| now.duration_since(t) < RESTART_WINDOW);
        attempts.push(now);
        let next = if attempts.len() <= 1 {
            RESTART_BACKOFF_MIN
        } else {
            let prev = *self.backoff.get(&id).unwrap_or(&RESTART_BACKOFF_MIN);
            (prev * 2).min(RESTART_BACKOFF_MAX)
        };
        self.backoff.insert(id, next);
        next
    }
}
```

- [ ] **Step 1: Write the failing tests**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allows_up_to_five_restarts_in_window() {
        let mut budget = RestartBudget::new();
        let base = Instant::now();
        for _ in 0..5 {
            assert!(budget.may_restart(ComponentId::Qso, base));
            budget.record_attempt_and_backoff(ComponentId::Qso, base);
        }
        assert!(!budget.may_restart(ComponentId::Qso, base));
    }

    #[test]
    fn window_expiry_resets_the_budget() {
        let mut budget = RestartBudget::new();
        let base = Instant::now();
        for _ in 0..5 {
            budget.record_attempt_and_backoff(ComponentId::Qso, base);
        }
        assert!(!budget.may_restart(ComponentId::Qso, base));
        let later = base + Duration::from_secs(601);
        assert!(budget.may_restart(ComponentId::Qso, later));
    }

    #[test]
    fn backoff_doubles_up_to_the_cap() {
        let mut budget = RestartBudget::new();
        let base = Instant::now();
        let b1 = budget.record_attempt_and_backoff(ComponentId::Qso, base);
        assert_eq!(b1, RESTART_BACKOFF_MIN);
        let b2 = budget.record_attempt_and_backoff(ComponentId::Qso, base + Duration::from_secs(1));
        assert_eq!(b2, RESTART_BACKOFF_MIN * 2);
        // Keep doubling past the cap — must clamp, never exceed MAX.
        for i in 3..10 {
            let b = budget.record_attempt_and_backoff(
                ComponentId::Qso,
                base + Duration::from_secs(i),
            );
            assert!(b <= RESTART_BACKOFF_MAX);
        }
    }

    #[test]
    fn components_have_independent_budgets() {
        let mut budget = RestartBudget::new();
        let base = Instant::now();
        for _ in 0..5 {
            budget.record_attempt_and_backoff(ComponentId::Qso, base);
        }
        assert!(!budget.may_restart(ComponentId::Qso, base));
        assert!(budget.may_restart(ComponentId::DxCluster, base));
    }
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta restart_budget:: --lib`
Expected: FAIL — module doesn't exist yet.

- [ ] **Step 3: Implement** the module above, register it in `mod.rs` with `mod restart_budget;`
  near the other `mod` declarations (`pub(crate) use restart_budget::RestartBudget;` if `mod.rs`'s
  existing convention re-exports its submodule types — check the neighboring `mod` lines for the
  pattern and match it).

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta restart_budget:: --lib`
Expected: PASS, 4/4.

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/restart_budget.rs pancetta/src/coordinator/mod.rs
git commit -m "feat(coordinator): RestartBudget — 5 restarts/10min rolling window, capped-exponential backoff"
```

---

## Task 4: Make `emit_diagnostic` reusable from `health.rs`

**Files:**
- Modify: `pancetta/src/coordinator/tx.rs` (the existing `async fn emit_diagnostic` at line 971 —
  change visibility only)
- Modify: `pancetta/src/coordinator/health.rs` (import it)

**Interfaces:**
- Produces: `pub(crate) async fn emit_diagnostic(message_bus: &MessageBus, target: &'static str, level: pancetta_core::DiagnosticLevel, text: String, qso_id: Option<&str>)` — same signature as today, just visible outside `tx.rs`.

- [ ] **Step 1: Change visibility**

In `pancetta/src/coordinator/tx.rs`, change:
```rust
async fn emit_diagnostic(
```
to:
```rust
pub(crate) async fn emit_diagnostic(
```

- [ ] **Step 2: Verify nothing else needs updating**

Run: `cargo build -p pancetta 2>&1 | tail -20`
Expected: builds clean (visibility widening never breaks existing callers).

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/coordinator/tx.rs
git commit -m "refactor(coordinator): widen emit_diagnostic visibility for reuse by the task supervisor"
```

---

## Task 5: Restructure `check_task_handles` into the supervisor loop

**Files:**
- Modify: `pancetta/src/coordinator/health.rs` (`check_task_handles`, currently lines 442-505+ —
  re-verify exact end line, the function continues past what's quoted in the spec)
- Modify: `pancetta/src/coordinator/mod.rs` (add a `restart_budget: RestartBudget` field to the
  coordinator struct, initialized in its constructor alongside `named_task_handles: Vec::new()`
  at line 1319)
- Test: `health.rs`'s test module, or a new integration test if `health.rs` has none today —
  check first with `grep -n "#\[cfg(test)\]" pancetta/src/coordinator/health.rs`.

**Interfaces:**
- Consumes: Task 1's `get_or_create_channel` — **not yet wired anywhere as of Task 1's commit**;
  this task's Step 0 below wires it into the 5 restartable components' own `create_channel` call
  sites, which is what actually makes restart work (a restarted component that still calls the
  plain `create_channel` will error on re-registration and the "restart" will silently fail every
  time). Also consumes Task 2's `component_restart_policy`, Task 3's `RestartBudget`, Task 4's
  `emit_diagnostic`.
- Produces: the restructured `check_task_handles` dispatches restart via a new private helper:

```rust
/// Re-invoke the given component's start method. Only the 5 components this
/// plan covers are wired; anything else reaching here is a bug (Task 2's
/// `component_restart_policy` should have already routed it to DegradeOnly).
async fn restart_component(&mut self, id: ComponentId) -> anyhow::Result<()> {
    match id {
        ComponentId::Autonomous => self.start_autonomous_component().await,
        ComponentId::DxCluster => self.start_dx_cluster_component().await,
        ComponentId::PskReporter => self.start_pskreporter_component().await,
        ComponentId::RemoteGateway => self.start_remote_gateway_component().await,
        ComponentId::Qso => self.start_qso_component().await,
        other => anyhow::bail!("restart_component called for non-restartable {other}"),
    }
}
```

- [ ] **Step 1: Wire Task 1's `get_or_create_channel` into the 5 restartable components (do this FIRST — the integration test in Step 2 depends on it to be meaningful)**

Verified current call sites (re-verify line numbers, may have shifted since this plan was
written): `pancetta/src/coordinator/autonomous.rs:529`, `pancetta/src/coordinator/dx_cluster.rs:26`
and `:55` (two sites — the disabled-config drain branch and the enabled branch, both create
`ComponentId::DxCluster`), `pancetta/src/coordinator/psk_reporter.rs:37` and `:85` (same
disabled/enabled two-site pattern for `ComponentId::PskReporter`),
`pancetta/src/coordinator/remote_gateway/mod.rs:285`, `pancetta/src/coordinator/qso.rs:1115`.

In each of these 6 call sites, change `.create_channel(ComponentId::X)` to
`.get_or_create_channel(ComponentId::X)` — mechanical, same `.await?` shape, no other change.

**Do NOT touch** `pancetta/src/coordinator/autonomous.rs:2069` (and its two neighboring
`create_channel` calls in the same block) — that one is test-fixture setup code for a *different*
component's test (per its own comment, "`start_autonomous_component` only ever creates its OWN
channel"), not a restart-path call site.

- [ ] **Step 2: Write the failing integration test**

```rust
#[tokio::test]
async fn panicking_restartable_component_is_restarted_after_backoff() {
    // Build a minimal ApplicationCoordinator (or the smallest fixture the
    // existing health.rs/mod.rs tests already use — grep for an existing
    // `fn test_coordinator()`/`fn make_coordinator()` helper and reuse it;
    // do not hand-construct the full struct if a helper already exists).
    let mut coordinator = test_coordinator().await;

    // Pre-create DxCluster's channel, simulating "this component already ran
    // once" — this is the scenario that actually exercises Step 1's fix.
    // Without it, this test would pass even if the restart path still called
    // the plain `create_channel` (a fresh coordinator has no registration
    // yet, so the first-ever call always succeeds regardless of which method
    // is used) — the bug this test must catch only manifests on the SECOND
    // registration attempt for the same ComponentId.
    coordinator
        .message_bus
        .create_channel(ComponentId::DxCluster)
        .await
        .unwrap();

    // Spawn a fake task under ComponentId::DxCluster that panics immediately.
    let handle = tokio::spawn(async {
        panic!("injected test panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    coordinator.named_task_handles.push((ComponentId::DxCluster, handle));

    // First poll: detects the panic, classifies it, restarts (DxCluster's
    // real start_dx_cluster_component runs — it must tolerate being invoked
    // in the test fixture; if it has hard external dependencies, use the
    // fixture pattern the existing dx_cluster.rs tests already use instead
    // of the production entry point).
    coordinator.check_task_handles().await;

    let status = coordinator.component_status.read().await;
    // After a successful restart, the component is Running again, not Failed.
    // Fails today (even after Step 1) without Step 3's restructure, because
    // check_task_handles doesn't restart anything yet; fails BEFORE Step 1
    // with a channel-already-exists error surfaced as a Failed restart.
    assert_eq!(
        status.get(&ComponentId::DxCluster).map(|s| &s.state),
        Some(&ComponentState::Running)
    );
}
```

Adapt this to whatever test-fixture pattern `health.rs` or `mod.rs` already uses for constructing
a coordinator in tests — grep for existing `#[tokio::test]` functions in `health.rs` first and
mirror their setup exactly rather than inventing a new one.

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p pancetta panicking_restartable_component --lib`
Expected: FAIL — today's `check_task_handles` never restarts, so `state` stays `Failed`.

- [ ] **Step 4: Restructure `check_task_handles`**

Replace the by-reference loop with a drain-by-index loop. The key structural change: collect
indices of finished handles first (immutable borrow), then process them one at a time by
removing from the vec (which needs `&mut self` for the restart dispatch too):

```rust
pub(crate) async fn check_task_handles(&mut self) {
    let finished_indices: Vec<usize> = self
        .named_task_handles
        .iter()
        .enumerate()
        .filter(|(_, (id, handle))| *id != ComponentId::Coordinator && handle.is_finished())
        .map(|(i, _)| i)
        .collect();

    // Remove from the back forward so earlier indices stay valid.
    for &i in finished_indices.iter().rev() {
        let (component_id, handle) = self.named_task_handles.remove(i);
        self.handle_finished_task(component_id, handle).await;
    }

    // Unchanged: update last_seen for still-running handles.
    for (component_id, handle) in &self.named_task_handles {
        if !handle.is_finished() {
            let mut status_map = self.component_status.write().await;
            if let Some(status) = status_map.get_mut(component_id) {
                if status.state == ComponentState::Running {
                    status.last_seen = Instant::now();
                }
            }
        }
    }
}

async fn handle_finished_task(
    &mut self,
    component_id: ComponentId,
    handle: tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let outcome = handle.await;
    let is_clean_exit = matches!(outcome, Ok(Ok(())));

    {
        let mut status_map = self.component_status.write().await;
        let status = status_map
            .entry(component_id)
            .or_insert_with(ComponentStatus::new_running);
        if status.state != ComponentState::Running {
            return; // already recorded this failure/restart-exhaustion
        }
        status.error_count += 1;
    }

    if is_clean_exit {
        // Intentional exit (e.g. a disabled component's drain task) — do
        // not restart, do not mark Failed either; leave as NotStarted-ish
        // by simply not re-adding a handle. Log at info, not warn/error.
        info!("Component {} exited cleanly, not restarting", component_id);
        return;
    }

    let degradation = degradation_message(component_id);
    // Preserve the existing Hamlib PTT-off safety behavior verbatim.
    if component_id == ComponentId::Hamlib {
        warn!("PTT safety: forcing PTT off due to Hamlib disconnect");
        let ptt_off_msg = ComponentMessage::new(
            ComponentId::Coordinator,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetPtt {
                state: false,
            }),
            Instant::now(),
        );
        let _ = self.message_bus.send_message(ptt_off_msg).await;
    }

    match component_restart_policy(component_id) {
        RestartPolicy::Restartable if self.restart_budget.may_restart(component_id, Instant::now()) => {
            let backoff = self
                .restart_budget
                .record_attempt_and_backoff(component_id, Instant::now());
            crate::coordinator::tx::emit_diagnostic(
                &self.message_bus,
                "supervisor",
                pancetta_core::DiagnosticLevel::Warn,
                format!(
                    "{} {} — restarting in {:?}",
                    component_id, degradation, backoff
                ),
                None,
            )
            .await;
            tokio::time::sleep(backoff).await;
            match self.restart_component(component_id).await {
                Ok(()) => {
                    info!("Component {} restarted successfully", component_id);
                }
                Err(e) => {
                    error!("Component {} restart failed: {}", component_id, e);
                    let mut status_map = self.component_status.write().await;
                    if let Some(status) = status_map.get_mut(&component_id) {
                        status.state = ComponentState::Failed(degradation.to_string());
                    }
                }
            }
        }
        RestartPolicy::Restartable => {
            // Budget exhausted — degrade instead of restarting again.
            warn!(
                "Component {} exceeded restart budget — degrading",
                component_id
            );
            crate::coordinator::tx::emit_diagnostic(
                &self.message_bus,
                "supervisor",
                pancetta_core::DiagnosticLevel::Error,
                format!(
                    "{} restarted too many times — giving up, needs a manual restart",
                    component_id
                ),
                None,
            )
            .await;
            let mut status_map = self.component_status.write().await;
            if let Some(status) = status_map.get_mut(&component_id) {
                status.state = ComponentState::Failed(degradation.to_string());
            }
        }
        RestartPolicy::DegradeOnly | RestartPolicy::FatalAbort => {
            error!(
                "Component {} has stopped unexpectedly: {}",
                component_id, degradation
            );
            let mut status_map = self.component_status.write().await;
            if let Some(status) = status_map.get_mut(&component_id) {
                status.state = ComponentState::Failed(degradation.to_string());
            }
        }
    }
}
```

Note: `restart_component` (defined at plan's Interfaces section above) goes in the same `impl`
block. `ComponentCriticality`-based log-level branching from the original code is folded into the
`DegradeOnly | FatalAbort` arm above (re-verify the original `Important`/`NonCritical` split is
preserved in spirit — if a test pins the exact original log text, keep it, adjusting only what's
needed for the restructure).

- [ ] **Step 5: Add the `restart_budget` field**

In `mod.rs`, add `restart_budget: RestartBudget,` to the coordinator struct near
`named_task_handles` (line 592), and `restart_budget: RestartBudget::new(),` in the constructor
near `named_task_handles: Vec::new()` (line 1319).

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p pancetta panicking_restartable_component --lib`
Expected: PASS

- [ ] **Step 7: Full workspace check**

Run: `cargo test --workspace --features transmit 2>&1 | tail -30`
Expected: all green, no regressions in existing `health.rs`/`mod.rs` tests.

- [ ] **Step 8: Commit**

```bash
git add pancetta/src/coordinator/health.rs pancetta/src/coordinator/mod.rs
git commit -m "feat(coordinator): restart-on-panic supervisor for the cheap-restartable component set"
```

---

## Task 6: QSO-drop surfacing on Qso restart

**Files:**
- Modify: `pancetta-qso/src/states.rs` (add `QsoFailureReason::SupervisorRestart` variant, near
  the existing 9 variants at line 140)
- Modify: `pancetta-tui/src/ui/mod.rs` (`recent_qso_failure_color`, line 1388 — add the new match
  arm; this is currently an exhaustive match with no wildcard, so the compiler forces this)
- Modify: `pancetta/src/coordinator/mod.rs` (new field to hold a `QsoManager` handle clone)
- Modify: `pancetta/src/coordinator/qso.rs` (`start_qso_component`, near its existing
  `qso_manager.clone()` pattern at line 1568 — store a clone into the new coordinator field)
- Modify: `pancetta/src/coordinator/health.rs` (`handle_finished_task`'s `Qso` restart path —
  enumerate active QSOs from the held clone before restarting, emit `QsoFailed` for each)
- Test: `pancetta-qso`'s test module for the new variant's `Display`/serialization if one exists
  (check `states.rs` for existing per-variant tests); `health.rs` integration test for the
  drop-surfacing behavior.

**Interfaces:**
- Consumes: `pancetta_qso::QsoManager::get_active_qsos()` (already exists, confirmed at
  `qso.rs:715`), `QsoManager::clone()` (confirmed cheap Arc-based handle clone,
  `qso_manager.rs:4410-4424`).
- Produces: `pancetta_qso::QsoFailureReason::SupervisorRestart` variant; a new coordinator field
  `qso_manager_for_supervisor: Option<pancetta_qso::QsoManager>` (mirrors the existing
  `wsjtx_qso_events_rx` field's doc-comment style — document why it's populated inside
  `start_qso_component` and read from the supervisor, same pattern).

- [ ] **Step 1: Write the failing test for the new enum variant**

```rust
// In pancetta-qso/src/states.rs's existing test module (or create one if none exists —
// check first with `grep -n "#\[cfg(test)\]" pancetta-qso/src/states.rs`)
#[test]
fn supervisor_restart_is_a_qso_failure_reason() {
    let reason = QsoFailureReason::SupervisorRestart;
    // Whatever trait/property every other variant has (Display, PartialEq, etc.) —
    // check the existing variants' test coverage and mirror it exactly, e.g. if there's
    // a `fn description(&self) -> &str` or similar, add SupervisorRestart's case and
    // test it here rather than inventing new test shape.
    assert_eq!(reason, QsoFailureReason::SupervisorRestart);
}
```

- [ ] **Step 2: Add the variant**

In `pancetta-qso/src/states.rs`, add after the existing 9 variants (verify the exact list first —
the design spec cites `Timeout, SignalLost, Duplicate, InvalidCallsign, FrequencyConflict,
UserCancelled, Superseded, StationQrt, ProtocolError`):

```rust
    /// The QSO engine's task was restarted by the coordinator's supervisor
    /// (a panic or crash mid-QSO), dropping this QSO's in-flight state. Not
    /// the operator's or the DX station's fault — surfaced distinctly so
    /// the Recent-QSOs panel doesn't misattribute it as e.g. a Timeout.
    SupervisorRestart,
```

Wherever `QsoFailureReason` derives `Display`/`Debug`/etc., ensure the derive covers the new
variant automatically (if it's a hand-written `impl Display` with a match, add the arm — check
first).

- [ ] **Step 3: Add the color-match arm (compiler-forced)**

In `pancetta-tui/src/ui/mod.rs`, `recent_qso_failure_color` (line 1388):

```rust
fn recent_qso_failure_color(reason: &pancetta_qso::QsoFailureReason) -> Color {
    use pancetta_qso::QsoFailureReason as R;
    match reason {
        R::Timeout | R::SignalLost | R::StationQrt | R::SupervisorRestart => Color::Yellow,
        R::InvalidCallsign | R::FrequencyConflict | R::ProtocolError(_) => Color::Red,
        R::Duplicate | R::UserCancelled | R::Superseded => Color::Gray,
    }
}
```

(`Yellow`, grouped with `Timeout`/`SignalLost` — a system-side interruption, not an operator or
protocol error, matching the existing color-grouping rationale.)

- [ ] **Step 4: Run tests to verify Steps 1-3 pass and nothing else broke**

Run: `cargo test -p pancetta-qso --lib && cargo test -p pancetta-tui --lib`
Expected: PASS. If `recent_qso_failure_color`'s own existing test enumerates all variants
exhaustively (check `pancetta-tui/src/ui/mod.rs`'s test module), it needs a new assertion for
`SupervisorRestart` too — add it in this step, not deferred.

- [ ] **Step 5: Commit the type change**

```bash
git add pancetta-qso/src/states.rs pancetta-tui/src/ui/mod.rs
git commit -m "feat(qso): add QsoFailureReason::SupervisorRestart"
```

- [ ] **Step 6: Write the failing integration test for drop-surfacing**

```rust
// In health.rs's test module, alongside Task 5's panicking_restartable_component test.
#[tokio::test]
async fn qso_restart_emits_supervisor_restart_failure_for_each_active_qso() {
    let mut coordinator = test_coordinator().await;
    coordinator.start_qso_component().await.unwrap();
    // Drive at least one QSO into an active state via whatever fixture helper
    // the existing qso.rs tests use (grep for `fixture_progress()` or similar,
    // confirmed to exist at qso.rs:4205 in this session's item-6 work) — insert
    // it into the live QsoManager the coordinator now holds a clone of.
    let manager = coordinator.qso_manager_for_supervisor.clone().unwrap();
    // ... insert an active QSO via the manager's real public API (grep for how
    // qso.rs's own tests seed an active QSO; use that exact method) ...

    // Simulate a Qso-task panic.
    let handle = tokio::spawn(async {
        panic!("injected test panic");
        #[allow(unreachable_code)]
        Ok(())
    });
    coordinator.named_task_handles.retain(|(id, _)| *id != ComponentId::Qso);
    coordinator.named_task_handles.push((ComponentId::Qso, handle));

    coordinator.check_task_handles().await;

    // Assert a QsoFailed{reason: SupervisorRestart} was emitted for the
    // seeded QSO — subscribe to the bus/QsoManager's event_sender BEFORE
    // triggering the restart and assert on what's received, mirroring
    // however Task 1's item-6 RecentQsoOutcome test asserted on bus output
    // (commit 5cdafd17 in this session's earlier work — same pattern).
}
```

- [ ] **Step 7: Implement the enumeration + emission**

In `mod.rs`, add the field (near `wsjtx_qso_events_rx`):

```rust
/// A cheap clone of the live `QsoManager` handle, stored so the task
/// supervisor (health.rs) can enumerate in-flight QSOs and surface them as
/// `QsoFailed{SupervisorRestart}` after the Qso component's task dies —
/// `QsoManager::clone()` shares the same `Arc<RwLock<..>>`-backed QSO map,
/// so this stays valid even after the original task that constructed it
/// has panicked. Populated by `start_qso_component`, overwritten on every
/// (re)start.
pub(crate) qso_manager_for_supervisor: Option<pancetta_qso::QsoManager>,
```

In `qso.rs`'s `start_qso_component`, right after `let mut qso_manager = pancetta_qso::QsoManager::new(qso_config);` (line 1260), add:

```rust
self.qso_manager_for_supervisor = Some(qso_manager.clone());
```

In `health.rs`'s `handle_finished_task`, before the `RestartPolicy::Restartable` match arm's
restart logic runs for `ComponentId::Qso` specifically, enumerate and emit:

```rust
if component_id == ComponentId::Qso {
    if let Some(manager) = self.qso_manager_for_supervisor.clone() {
        for qso in manager.get_active_qsos().await {
            manager.emit_qso_failed(qso.id, pancetta_qso::QsoFailureReason::SupervisorRestart).await;
            // ^ Use whatever the real existing method is for the QSO
            //   engine to publish a QsoFailed event — grep for how
            //   coordinator/qso.rs or qso_manager.rs currently transitions
            //   a QSO to Failed and reuse that exact call, adjusting only
            //   the reason. Do not hand-construct the QsoEvent variant
            //   directly if a manager method already does it correctly
            //   (state transition + event emission together).
        }
    }
}
```

(The exact method name for "transition a QSO to Failed and emit the event" needs to be found by
reading `qso_manager.rs`'s public API before writing this — grep `pub async fn.*fail\|pub async
fn.*Failed` in `pancetta-qso/src/qso_manager.rs` and use whatever exists; if nothing fits, this
step needs a NEED_CONTEXT escalation rather than inventing a new state-transition path.)

- [ ] **Step 8: Run test to verify it passes**

Run: `cargo test -p pancetta qso_restart_emits_supervisor_restart --lib`
Expected: PASS

- [ ] **Step 9: Full workspace check**

Run: `cargo test --workspace --features transmit 2>&1 | tail -30`

- [ ] **Step 10: Commit**

```bash
git add pancetta/src/coordinator/mod.rs pancetta/src/coordinator/qso.rs pancetta/src/coordinator/health.rs
git commit -m "feat(coordinator): surface dropped in-flight QSOs on supervisor restart (SupervisorRestart)"
```

---

## Task 7: Docs + final gate

**Files:**
- Modify: `CLAUDE.md` (one-line bullet: task supervision now auto-restarts the 5 cheap-restartable
  components; keep under the ~100-line budget — trim something if needed)
- Modify: `docs/superpowers/specs/2026-07-25-task-supervision-design.md` (status header: mark
  Phase 1 shipped, note Dsp/Ft8Decoder/Hamlib/StationAgent/Audio as the explicit follow-on)

- [ ] **Step 1: Write the docs**
- [ ] **Step 2: Full workspace suite one final time:** `cargo test --workspace --features transmit`
- [ ] **Step 3: `cargo fmt --check` and `cargo clippy --workspace --exclude pancetta-research --features transmit`**
- [ ] **Step 4: Commit docs; controller pushes the batch and opens a PR.**

---

## Self-review notes (author)

- Spec coverage: §4.1 (RestartPolicy) → Task 2; §4.2 (supervisor loop) → Task 5; §4.3 (retry
  budget) → Task 3; §4.4 (QSO-drop surfacing) → Task 6; §4.5 (channel idempotency) → Task 1; §4.6
  (panic surface) → no task, already shipped, explicitly left alone.
- Explicit scope cut from the spec, discovered during plan-writing: the spec assumed a uniform
  "12 `start_*_component` methods, all `&mut self`" restart surface. Verified against real source
  that Dsp/Ft8Decoder need moved-once channel/atomic parameters (not restartable without new
  re-supply plumbing) and Hamlib/StationAgent/Audio need teardown semantics not yet designed in
  code (only sketched in the spec). This plan covers exactly the 5 components with real, verified
  no-argument `&mut self -> Result<()>` start methods. Follow-on plans needed for the rest —
  tracked in memory, not silently dropped.
- Real discovery grounding Task 6: `QsoManager::clone()` (confirmed at `qso_manager.rs:4410`) is
  a cheap `Arc`-based handle clone, not a deep clone — this is what makes post-death QSO
  enumeration possible without new shared-state infrastructure beyond one held clone.
- Type consistency: `RestartPolicy` (Task 2) consumed by Task 5; `RestartBudget` (Task 3)
  consumed by Task 5; `emit_diagnostic` (Task 4) consumed by Task 5; `QsoFailureReason::
  SupervisorRestart` (Task 6) consumed by the same task's `handle_finished_task` addition.
