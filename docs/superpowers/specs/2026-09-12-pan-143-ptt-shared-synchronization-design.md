# PAN-143: Real shared synchronization for PTT-assertion / offset-switch commit — design

**Ticket:** PAN-143 (parent PAN-134). PR #369 (PAN-140) round 2 found that
`apply_tx_offset_switch`'s PTT-in-flight recheck (`pancetta-qso/src/qso_manager.rs`, immediately
before the frequency mutation) only *narrows* the race against the TX worker's own
`ptt_active.store(true, Release)` (`pancetta/src/coordinator/tx.rs`, `PttGuard::new`) — it cannot
*close* it, because the TX worker never shares a lock with the QSO write lock it recheck-under.
The identical shape exists for the pre-existing Hold-mode recheck a few lines above (`tx_freq_mode`,
stored independently by the TUI-relay task's `ToggleTxFreqMode` handler,
`pancetta/src/coordinator/tui_relay.rs`). PAN-134 (parent) names the general root cause:
check-then-act against state owned by a different subsystem, with no shared lock, so a recheck
immediately before the gated action can only shrink the window, never close it. This spec covers
one mechanism applied to **both** sites at once, per the ticket's explicit ask.

## Why a recheck can never close this

`apply_tx_offset_switch` already re-reads `ptt_active`/`tx_freq_mode` *inside* its own
`self.qsos.write().await` lock, right before mutating `progress.metadata.frequency`. That closes
races against anything **also serialized through the `qsos` lock** — but `ptt_active` and
`tx_freq_mode` are plain atomics written by two entirely separate tasks (the TX worker, the
TUI-relay task) that never touch `qsos` at all. A load-then-branch on one side racing a
store on the other, with no third primitive tying them together, is fundamentally a TOCTOU gap no
matter how close together the load and the branch are — adding more atomics (a generation counter,
a compare-and-swap) does not change this: it is still two independent reads/writes with no
happens-before relationship *unless* both sides serialize through one shared primitive. That is
mutual exclusion, not a narrower check.

## Chosen mechanism: a dedicated shared mutex, not the QSO write lock itself

Add one new, narrow primitive: `ptt_sync_gate: Arc<std::sync::Mutex<()>>`. Both critical sections
below acquire it for the few synchronous instructions that read-then-write the shared flags. No
`.await` ever occurs while it is held (verified per call site below), so a **blocking**
`std::sync::Mutex` is correct and simpler than an async `tokio::sync::Mutex` — it needs no `.await`
plumbing through `PttGuard::new` or the TUI-relay handler, keeping both call sites synchronous exactly
as they are today.

**Why not reuse `QsoManager`'s own `qsos: RwLock<HashMap<QsoId, QsoProgress>>` directly (option
(a) read literally)?** That would require exposing `pancetta-qso`'s internal map lock to
`pancetta`'s TX worker and TUI-relay task, a new and much larger coupling in the wrong direction
(those tasks have zero `QsoManager` reference today — see "Rejected alternatives"). A
purpose-built unit-guarded mutex gives the *same* mutual-exclusion guarantee for exactly the two
flags that need it, without entangling unrelated internal state or crate boundaries.

**Why not a generation-token/CAS protocol (option (c)) instead of a lock?** Worked through
concretely: the TX worker would capture a generation, do its own work, then compare-and-swap
before storing `ptt_active`. But the *comparison* and the *store* are still two operations with no
atomicity between them unless they happen under a lock — which is exactly this mechanism, just
with extra bookkeeping. A raw CAS on `ptt_active` itself (`compare_exchange` instead of `store`)
doesn't help either: `apply_tx_offset_switch`'s failure mode isn't "we clobbered each other's write
to the same bool", it's "the two sides observed each other's state at genuinely different
instants with nothing to make those instants coincide." Only mutual exclusion fixes that.

**Why not move the offset-switch commit onto the TX worker's own execution context (option (b))?**
`pancetta` depends on `pancetta-qso`, never the reverse; the offset-switch logic (allocator
interplay, Hound-region re-derivation, `pre_switch_offset` bookkeeping) is `QsoManager`-owned by
design and must stay there. Moving it into `tx.rs` would invert an existing dependency direction
for a correctness fix that doesn't require it.

## Call sites

### 1. `QsoManager::apply_tx_offset_switch` (`pancetta-qso/src/qso_manager.rs`)

Already holds `self.qsos.write().await` when it reaches guards 5 (Hold-mode) and 6
(PTT-in-flight), around line 3652–3673. Wrap exactly that span — the two flag rechecks plus the
`progress.metadata.frequency = applied_hz` (and the two adjacent one-line resets,
`pending_freq_drift = None`, `stall_cycles = 0`, part of the same logical commit) — in an explicit
block that locks and drops `ptt_sync_gate` before falling through to the rest of the function
(the `partner_freq` latch, the PAN-72 one-shot resend, logging). Those later statements read
`progress`/`applied_hz`/`old_off` but never re-read `ptt_active`/`tx_freq_mode`, so they don't need
to be inside the gate, and the function's later `.await` points (event emission) must not happen
while a blocking mutex is held — confirmed by reading the rest of the function.

```rust
{
    let _ptt_sync = self.ptt_sync_gate.lock().unwrap_or_else(|p| p.into_inner());
    if !pancetta_core::TxFreqMode::from_u8(self.tx_freq_mode.load(Ordering::Relaxed))
        .allows_auto_change()
    {
        return Err(QsoManagerError::OffsetActionHeld { qso_id });
    }
    if self.ptt_active.load(Ordering::Acquire) {
        return Err(QsoManagerError::OffsetActionPttInFlight { qso_id });
    }
    progress.metadata.frequency = applied_hz;
    progress.metadata.pending_freq_drift = None;
    progress.metadata.stall_cycles = 0;
} // _ptt_sync dropped here, well before any subsequent .await
```

(An early `return` from inside the block still drops the `MutexGuard` via normal Rust scope
rules — no explicit unlock needed on the error paths.)

New field `ptt_sync_gate: Arc<std::sync::Mutex<()>>` on `QsoManager`, defaulted to a fresh mutex in
`QsoManager::new` (mirrors `ptt_active`'s own `AtomicBool::new(false)` default), with a new setter
`set_ptt_sync_gate_source` mirroring `set_ptt_active_source`/`set_tx_freq_mode_source` exactly —
same doc-comment convention, same "coordinator wiring" note.

### 2. `PttGuard::new` (`pancetta/src/coordinator/tx.rs`)

Add one parameter, `ptt_sync_gate: &std::sync::Arc<std::sync::Mutex<()>>`. Wrap only the
`ptt_active.store(true, Release)` call:

```rust
fn new(
    message_bus: MessageBus,
    ptt_active: std::sync::Arc<std::sync::atomic::AtomicBool>,
    ptt_sync_gate: &std::sync::Arc<std::sync::Mutex<()>>,
    last_ptt_on_ms: &std::sync::Arc<std::sync::atomic::AtomicU64>,
) -> Self {
    {
        let _ptt_sync = ptt_sync_gate.lock().unwrap_or_else(|p| p.into_inner());
        ptt_active.store(true, std::sync::atomic::Ordering::Release);
    }
    last_ptt_on_ms.store(super::now_epoch_ms(), std::sync::atomic::Ordering::Release);
    Self { message_bus, armed: true, ptt_active }
}
```

`last_ptt_on_ms` stays outside the gate — it's a diagnostic timestamp for the desense monitor, not
part of the race this ticket closes, and there is no benefit to serializing it. `PttGuard::new`
stays a plain (non-`async`) fn — no `.await` is introduced anywhere in this fix, so nothing
downstream needs to change from sync to async.

The coordinator struct in `pancetta/src/coordinator/mod.rs` gains one field, `ptt_sync_gate:
Arc<std::sync::Mutex<()>>`, initialized alongside `ptt_active` (`Arc::new(std::sync::Mutex::new(()))`).
The TX-worker spawn closure (around `mod.rs`'s `tx_handle` construction) clones it in alongside
`ptt_active`/`last_ptt_on_ms`, and all three `PttGuard::new` call sites in `tx.rs` pass
`&ptt_sync_gate`.

### 3. Every `tx_freq_mode` writer in `pancetta/src/coordinator/tui_relay.rs`

There are **three** call sites that bump the generation and store `tx_freq_mode`, not one — all
three need the same gate, or the fix is incomplete for exactly the two it misses:

- `ToggleTxFreqMode` (the `f` key), ~line 1815.
- `SetTxOffset { offset_hz: Some(hz) }` (the `o` modal, setting a hold), ~line 1842.
- `SetTxOffset { offset_hz: None }` (the `o` modal, clearing the hold), ~line 1864.

All three already share the identical "generation-bump-then-store, both `SeqCst`" shape
(deliberately paired for the unrelated PAN-38/39 race — see `ToggleTxFreqMode`'s own doc comment,
which the other two explicitly reference: "see ToggleTxFreqMode's comment above"). Wrap each:

```rust
{
    let _ptt_sync = ptt_sync_gate.lock().unwrap_or_else(|p| p.into_inner());
    cmd_tx_freq_mode_generation.fetch_add(1, Ordering::SeqCst);
    cmd_tx_freq_mode.store(next.as_u8(), Ordering::SeqCst); // or the Hold/Auto literal at the other two sites
}
```

This does not change the existing PAN-38/39 generation/mode read-pairing contract at all (still
"generation bump, then mode store", still `SeqCst`) — it only adds mutual exclusion against
`apply_tx_offset_switch`'s concurrent read of the same `tx_freq_mode` atomic. `tui_relay`'s spawn
context needs the same `ptt_sync_gate` Arc threaded in from the coordinator alongside
`cmd_tx_freq_mode`/`cmd_tx_freq_mode_generation`, in scope for all three match arms.

### Other `ptt_active` writers, deliberately not gated

`pancetta-hamlib`'s teardown/reconnect paths (`hamlib.rs`, two sites) also write `ptt_active` —
both `store(false, ...)` only, under the existing "Restart teardown guarantees PTT release for
Hamlib" invariant (AGENTS.md). A `false` store can never cause `apply_tx_offset_switch` to
wrongly *refuse* a switch (the check is `if ptt_active { refuse }`), and both only run during an
already-TX-inhibited teardown window, so they are intentionally left outside the gate — gating
them would add lock traffic to a shutdown path for no correctness benefit.

## Wiring

`pancetta/src/coordinator/qso.rs` already calls `set_tx_freq_mode_source`/`set_ptt_active_source`
on the freshly constructed `QsoManager` (around line 2570–2575) from the coordinator's own
`self.tx_freq_mode`/`self.ptt_active`. Add `set_ptt_sync_gate_source(self.ptt_sync_gate.clone())`
in the same place, same pattern.

## Lock-ordering / deadlock safety

`QsoManager::apply_tx_offset_switch` acquires `self.qsos.write().await` **first**, then
(synchronously, no intervening `.await`) `ptt_sync_gate`. Neither the TX worker (`PttGuard::new`)
nor the TUI-relay handler ever acquires `qsos` — they only ever take `ptt_sync_gate`. So the global
lock-acquisition order is strictly `qsos → ptt_sync_gate` on the one path that takes both, and
`ptt_sync_gate` alone everywhere else — no cycle exists, so no deadlock is possible regardless of
interleaving. This also means `ptt_sync_gate` contention is bounded by whichever of the three call
sites is fastest to acquire-and-release (a handful of atomic ops each, no I/O, no hardware calls,
no further locks) — negligible latency, and it does not serialize unrelated QSOs' TX scheduling
(each QSO's own transmission still proceeds independently once past this one instant).

## Poison handling

The guarded type is `()` — there is no data invariant a panic mid-critical-section could
corrupt, only the mutual-exclusion property itself, which is unaffected by poisoning. All three
call sites use `.lock().unwrap_or_else(|p| p.into_inner())` to recover and proceed rather than
propagate a panic — appropriate here specifically because there is no state to distrust after
recovery, unlike the armed-TX gate's documented fail-closed behavior on a poisoned lock (that gate
guards actual authorization state where a poison must not be silently trusted).

## Test impact

- `PttGuard::new` gains one parameter — touches its 3 production call sites in `tx.rs` and every
  test that constructs one directly. `tx.rs`'s existing test harness builds a
  `ptt_active: Arc<AtomicBool>` per test at 14 sites (verified by grep, no shared fixture); each
  gets one more `Arc::new(std::sync::Mutex::new(()))` alongside it — mechanical, no behavior
  change for tests that never race the gate.
- `QsoManager::set_ptt_sync_gate_source` is optional to call in tests exactly like
  `set_ptt_active_source`/`set_tx_freq_mode_source` already are — `QsoManager::new`'s default
  (a fresh, uncontended mutex) means every existing test that never injects a custom source keeps
  working unchanged.
- New unit tests (both crates): (1) `apply_tx_offset_switch` still refuses on `ptt_active == true`
  and on `tx_freq_mode == Hold` when read through the gate (regression-equivalent to the two
  existing `..._refuses_when_..._after_the_request_was_queued` tests — the gate must not change
  observable behavior in the *uncontested* case); (2) a genuine concurrency test: spawn a task
  that holds `ptt_sync_gate` and attempts `ptt_active.store(true, ...)` after the guard would have
  raced past a bare-atomic check, verifying `apply_tx_offset_switch` (called concurrently) blocks
  on the gate and observes the post-store value deterministically rather than racing it. This is
  the actual regression test for the bug PAN-140 round 2 found and this ticket closes.

## Non-goals

This spec does not address PAN-134's other named site (the arm-authorization
check-then-enqueue gap in `MessageBus::send_message`, PAN-138) — that is a different
subsystem (arm/authorization vs. frequency/PTT scheduling) and a different parent-ticket
follow-up, tracked separately.
