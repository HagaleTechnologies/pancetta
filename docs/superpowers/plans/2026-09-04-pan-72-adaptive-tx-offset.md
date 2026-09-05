# PAN-72: Adaptive TX-Offset Switching Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** In Auto TX-freq mode, generalize mid-QSO stuck detection to also catch total silence
(not just repeated frames), switch to a smart-allocator-picked open offset instead of a fixed
`+300Hz` hop, remember the last offset that actually worked and revert to it on a second stall,
and add a manual `u` keystroke that forces an immediate nudge (QSO or CQ offset) without leaving
Auto mode.

**Architecture:** `QsoManager` (pancetta-qso) owns all stall bookkeeping and metadata mutation
and emits a `QsoEvent` when an offset action is needed; it never touches the smart allocator
(single-scorer invariant). The coordinator (pancetta binary) relays that event into a
`Mutex<Vec<_>>` mailbox — the same shape already used for
`pending_autonomous_cq_dispatch_failures` — which the existing Autonomous task drains once per
15s tick, resolves via `AutonomousOperator::allocate_smart_frequency` (the sole allocator
caller), and commits back into `QsoManager` via one new external-mutation method. The manual
keystroke reuses the same mailbox for the QSO case and a one-shot consumed flag for the CQ case.

**Tech Stack:** Rust, Tokio (`Arc<Mutex<_>>`/`Arc<RwLock<_>>` shared state, `tokio::sync::broadcast`
for `QsoEvent`), `serde`/TOML config (`pancetta-config`), `crossterm` (TUI key events).

**Spec:** `docs/superpowers/specs/2026-09-04-pan-72-adaptive-tx-offset-design.md`

## Global Constraints

- **Single-scorer:** only `AutonomousOperator` (pancetta-qso/src/autonomous.rs) may call
  `SmartFrequencyAllocator`/`allocate_smart_frequency`. `QsoManager` must never gain its own
  allocator instance or spectral-state field.
- **`merge_with` must carry every config field** — any new field added to a `pancetta-config`
  section (`AutonomousConfig`) must get a line in that section's `merge_with`, or the
  `assert_carries_all` guardrail test in `pancetta-config/src/lib.rs` fails.
- **Auto-initiated QSO safety bounds are untouched:** `report_timeout` (30s),
  `AUTO_RESEND_MAX_CALLS` (2), `repetitive_tx_timeout_secs` (300s default) — do not change these
  constants or their gating logic anywhere in this plan.
- **Sticky QSO offsets:** a QSO's `metadata.frequency` is only ever changed by an explicit,
  intentional action (QSO open, Hound QSY, or this ticket's new switch/revert/nudge) — never
  silently re-optimized on a timer.
- **`repetitive_tx_timeout_secs` / manual watchdog behavior is untouched** — this plan only adds
  a new offset-action trigger; it does not change when a QSO is retired.
- Every new/changed public item needs a doc comment consistent with the surrounding file's style
  (this codebase documents *why*, not *what* — see any existing doc comment in the touched files
  for the expected tone).

---

### Task 1: `QsoMetadata` gains `stall_cycles` and `last_known_good_offset_hz`

**Files:**
- Modify: `pancetta-qso/src/states.rs:513-529` (the `last_rx_text`/`dx_repeat_count` fields and
  their doc comments)
- Modify: every `QsoMetadata { ... }` struct-literal construction site across the workspace that
  currently sets `last_rx_text` and `dx_repeat_count` explicitly (this codebase does not use
  `..Default::default()` spread syntax for `QsoMetadata` — every field is listed out at each
  construction site, so removing/renaming a field is a compile-error-driven mechanical sweep).
  Known sites as of this writing (find the authoritative, current list with the grep in Step 1 —
  do not trust this list blindly, it may drift):
  - `pancetta-qso/src/qso_manager.rs` (multiple sites, e.g. ~1031-1032, ~1165-1166, ~1559-1560,
    ~1918-1919, ~9000-9001, ~9547-9548, ~13723-13724, ~13958-13959)
  - `pancetta-qso/src/adif.rs` (~602-603, ~1241-1242)
  - `pancetta-qso/src/adif_log_writer.rs` (~196-197)
  - `pancetta-qso/src/async_logger.rs` (~973-974, ~1069-1070)
  - `pancetta-qso/src/async_database.rs` (~1297-1298, ~1422-1423, ~1537-1538)
  - `pancetta-qso/src/statistics.rs` (~2306-2307)
- Test: `pancetta-qso/src/qso_manager.rs` (inline `#[cfg(test)]` module — follow the existing
  test-module convention in that file)

**Interfaces:**
- Produces: `QsoMetadata::stall_cycles: u32` (replaces `dx_repeat_count`), `QsoMetadata::
  last_known_good_offset_hz: Option<f64>` (new). Both `#[serde(default)]` for forward-compat with
  already-persisted records (matches the existing `#[serde(default)] pub dx_repeat_count: u32`
  pattern being replaced — confirm no `#[serde(deny_unknown_fields)]` sits on `QsoMetadata`
  before removing the old fields; if it does, stop and flag it rather than proceeding, since that
  would break deserializing already-persisted QSO records).

- [ ] **Step 1: Find every construction site**

Run: `grep -rn "last_rx_text\|dx_repeat_count" pancetta-qso/src/ pancetta/src/` and confirm there
is no `#[serde(deny_unknown_fields)]` on `QsoMetadata`:
`grep -n "deny_unknown_fields" pancetta-qso/src/states.rs`
Expected: no output for the `deny_unknown_fields` check. If it DOES find something, stop this
task and re-read the spec's "No Placeholders" note above before proceeding — this plan assumes
it's absent.

- [ ] **Step 2: Replace the fields in `states.rs`**

In `pancetta-qso/src/states.rs`, replace (around lines 513-529):

```rust
    /// The last DX frame text received on this QSO (uppercased, trimmed).
    /// Paired with [`Self::dx_repeat_count`] to detect a DX that keeps sending
    /// the *same* message because it isn't copying our replies — the cue to
    /// move our TX frequency off a possible collision (the only on-air reason
    /// a held TX frequency would stop "working" mid-QSO; FT8 receivers decode
    /// the whole passband, so our offset only matters when something is
    /// stepping on it). `None` until the first DX frame.
    #[serde(default)]
    pub last_rx_text: Option<String>,

    /// Consecutive count of identical DX frames that did NOT advance the QSO.
    /// Reset to 0 on any forward state advance, and to 1 when a *different*
    /// non-advancing frame arrives. When it reaches the stuck-repeat threshold
    /// the QSO performs a one-time TX-frequency hop (then resets to 0). See
    /// [`Self::last_rx_text`].
    #[serde(default)]
    pub dx_repeat_count: u32,
```

with:

```rust
    /// Consecutive cycles since this QSO last made forward progress (a
    /// genuinely new/advancing DX message). Incremented once per ~15s slot
    /// while stalled — by [`QsoManager::rearm_manual_calls_at`] when we
    /// re-transmit without an advance, which covers BOTH total silence and a
    /// DX repeating the same non-advancing frame (the state simply stays the
    /// same either way). Reset to 0 on any forward state advance. See
    /// [`Self::last_known_good_offset_hz`] for what happens once this trips a
    /// switch (PAN-72).
    #[serde(default)]
    pub stall_cycles: u32,

    /// The TX audio offset this QSO was on the last time it made forward
    /// progress (a genuinely new/advancing DX message). `None` until the
    /// first advance. Used to decide, when [`Self::stall_cycles`] trips a
    /// switch: if we're currently ON this offset (or it's `None`), search for
    /// a new one; if we're currently on some OTHER (previously-switched)
    /// offset, revert to this one instead of searching again (PAN-72).
    #[serde(default)]
    pub last_known_good_offset_hz: Option<f64>,
```

- [ ] **Step 3: Fix every construction site found in Step 1**

At each site, replace the two lines (typically `last_rx_text: None,` and
`dx_repeat_count: 0,`, though verify each — a couple may differ in exact spacing) with:

```rust
            stall_cycles: 0,
            last_known_good_offset_hz: None,
```

- [ ] **Step 4: Build until clean**

Run: `cargo build -p pancetta-qso 2>&1 | grep -E "error|missing field"`
Expected: build succeeds with no "missing field" or "no field `dx_repeat_count`" errors. Fix any
remaining site the grep in Step 1 missed (the compiler will name the exact struct-literal it's
unhappy with).

- [ ] **Step 5: Run pancetta-qso's existing test suite**

Run: `cargo test -p pancetta-qso --features transmit`
Expected: PASS. Any test that directly set/asserted `dx_repeat_count`/`last_rx_text` (search the
same grep from Step 1 for hits inside `#[cfg(test)]` modules) will fail to compile — update those
assertions to use `stall_cycles` with equivalent intent, or delete them if they were specifically
about the old fixed-hop mechanism being removed in Task 4 below (leave a one-line comment noting
the removal and which task superseded it, don't just silently delete without a trace in the
commit message).

- [ ] **Step 6: Commit**

```bash
git add pancetta-qso/
git commit -m "$(cat <<'EOF'
refactor(qso): replace dx_repeat_count/last_rx_text with stall_cycles/last_known_good_offset_hz

PAN-72 groundwork: generalizes stuck-DX detection from "repeated identical
frame" to "no forward progress", and adds the state needed to revert to a
previously-working offset after a second stall.
EOF
)"
```

---

### Task 2: `TimeoutConfig::qso_stall_switch_after`

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs:343-389` (`TimeoutConfig` struct + its `Default` impl
  at ~536-548)
- Test: `pancetta-qso/src/qso_manager.rs` inline test module

**Interfaces:**
- Consumes: nothing new
- Produces: `TimeoutConfig::qso_stall_switch_after: u32` (default 4) — Task 4 reads
  `self.config.timeouts.qso_stall_switch_after`.

- [ ] **Step 1: Write the failing test**

Add near other `TimeoutConfig`-default tests in `qso_manager.rs`:

```rust
#[test]
fn timeout_config_default_qso_stall_switch_after_is_4() {
    let config = TimeoutConfig::default();
    assert_eq!(config.qso_stall_switch_after, 4);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-qso timeout_config_default_qso_stall_switch_after_is_4`
Expected: FAIL — `no field \`qso_stall_switch_after\` on type \`TimeoutConfig\`` (compile error).

- [ ] **Step 3: Add the field**

In `TimeoutConfig` (qso_manager.rs, right after `repetitive_tx_timeout_secs`, ~line 383):

```rust
    /// Consecutive stalled cycles (see [`crate::states::QsoMetadata::
    /// stall_cycles`]) before an in-progress QSO's TX offset is switched (or
    /// reverted to its last known-good offset) — Auto TX-freq mode only.
    /// Default 4 (PAN-72). Distinct from `AutonomousConfig::
    /// cq_no_response_switch_after` (a different struct, governs the
    /// self-CQ-hunting case before any QSO exists), though the two are
    /// exposed under the same `[autonomous]` TOML section by the coordinator
    /// (see `pancetta-config`) since they're one logical "adaptive TX
    /// behavior" knob from the operator's point of view.
    #[serde(default = "default_qso_stall_switch_after")]
    pub qso_stall_switch_after: u32,
```

Add the default fn right after `default_repetitive_tx_timeout_secs` (~line 389):

```rust
/// Default for [`TimeoutConfig::qso_stall_switch_after`] (PAN-72).
fn default_qso_stall_switch_after() -> u32 {
    4
}
```

Add it to `impl Default for TimeoutConfig` (~line 536-548):

```rust
            qso_stall_switch_after: default_qso_stall_switch_after(),
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-qso timeout_config_default_qso_stall_switch_after_is_4`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "feat(qso): add TimeoutConfig::qso_stall_switch_after (default 4)"
```

---

### Task 3: `OffsetAction` type and `QsoEvent::TxOffsetActionNeeded`

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs:444-513` (the `QsoEvent` enum)
- Test: `pancetta-qso/src/qso_manager.rs` inline test module

**Interfaces:**
- Produces: `pub enum OffsetAction { Switch { avoid_hz: f64 }, Revert { target_hz: f64 } }`
  (`#[derive(Debug, Clone, PartialEq)]`, matching the style of other small QSO-domain enums in
  this file), and `QsoEvent::TxOffsetActionNeeded { qso_id: QsoId, action: OffsetAction }`. Task
  4 constructs/emits these; Task 8 (coordinator) matches on them.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn offset_action_switch_and_revert_are_distinct() {
    let switch = OffsetAction::Switch { avoid_hz: 1500.0 };
    let revert = OffsetAction::Revert { target_hz: 1200.0 };
    assert_ne!(switch, revert);
    assert_eq!(switch, OffsetAction::Switch { avoid_hz: 1500.0 });
}

#[tokio::test]
async fn tx_offset_action_needed_event_is_constructible_and_broadcastable() {
    let config = QsoManagerConfig::default();
    let manager = QsoManager::new(config);
    let mut rx = manager.subscribe();
    let qso_id = QsoId::new_v4();
    manager
        .test_emit_event(QsoEvent::TxOffsetActionNeeded {
            qso_id,
            action: OffsetAction::Switch { avoid_hz: 1500.0 },
        })
        .await;
    let event = rx.recv().await.unwrap();
    match event {
        QsoEvent::TxOffsetActionNeeded { qso_id: id, action } => {
            assert_eq!(id, qso_id);
            assert_eq!(action, OffsetAction::Switch { avoid_hz: 1500.0 });
        }
        other => panic!("expected TxOffsetActionNeeded, got {:?}", other),
    }
}
```

If `QsoManager` has no existing test-only `emit_event` helper exposed to `#[cfg(test)]` code,
check how other tests in this file assert on emitted events (grep `manager.subscribe()` and
`self.emit_event(` in this file) and use whichever real, already-emitting code path an existing
test uses instead of inventing a `test_emit_event` helper — do not add test-only production API
surface if a real trigger path already exists to exercise it (Task 4 gives you one: drive
`rearm_manual_calls_at`/`process_message_for_qso` to the threshold and assert the emitted event
from the resulting broadcast, same as this test does structurally, just via the real path). Adapt
this step's test accordingly once Task 4 lands — it's fine for this task's test to be minimal
(just the `OffsetAction` equality test) if there's no clean way to emit the new event without
Task 4's logic; move the event-emission assertion into Task 4's tests in that case.

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-qso offset_action_switch_and_revert_are_distinct`
Expected: FAIL — `cannot find type \`OffsetAction\``.

- [ ] **Step 3: Add the type and event variant**

In `qso_manager.rs`, near the top of the file alongside other small public enums (check where
`RejectionReason` or similar small enums are defined and match that style/location):

```rust
/// What an in-progress QSO's stall-detection (PAN-72) wants done about its
/// TX offset. Emitted by `QsoManager` as `QsoEvent::TxOffsetActionNeeded`;
/// resolved and committed by the coordinator (see
/// `docs/superpowers/specs/2026-09-04-pan-72-adaptive-tx-offset-design.md`)
/// since only `AutonomousOperator` may call the smart allocator.
#[derive(Debug, Clone, PartialEq)]
pub enum OffsetAction {
    /// We're on the last known-good offset (or none is recorded yet) — find
    /// a new one, avoiding this one.
    Switch { avoid_hz: f64 },
    /// We stalled again on a previously-switched offset — go back to the one
    /// that was last confirmed working, no allocator call needed.
    Revert { target_hz: f64 },
}
```

Add to `QsoEvent` (after `MessageRejected`, before the closing `}` at ~line 512-513):

```rust
    /// PAN-72: an in-progress QSO's TX offset needs an autonomous action
    /// (Auto TX-freq mode only — see `QsoManager::rearm_manual_calls_at`).
    /// The coordinator resolves `OffsetAction::Switch` via the smart
    /// allocator and commits either variant via
    /// `QsoManager::apply_tx_offset_switch`.
    TxOffsetActionNeeded {
        qso_id: QsoId,
        action: OffsetAction,
    },
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-qso offset_action_switch_and_revert_are_distinct`
Expected: PASS. (Leave the second test from Step 1 for Task 4 if it doesn't have a clean trigger
path yet, per the note in Step 1.)

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "feat(qso): add OffsetAction and QsoEvent::TxOffsetActionNeeded"
```

---

### Task 4: Stall detection, known-good tracking, and event emission (replaces the old hop)

This is the core behavioral change. Read `pancetta-qso/src/qso_manager.rs` around these three
spots before starting — line numbers below are as of this plan's writing and may have shifted
slightly from Tasks 1-3's edits:

1. The removed mechanism: `DX_STUCK_REPEAT_THRESHOLD` (const, ~line 26), `STUCK_TX_HOP_HZ`
   (~line 31), `stuck_hopped_offset` (fn, ~lines 84-93), and the "Stuck-DX TX-frequency
   hold/escape" block inside `process_message_for_qso` (~lines 2985-3033, the block starting
   `// Stuck-DX TX-frequency hold/escape (operator request)` and ending after the `dx_repeat_count
   = 0;` `warn!` hop).
2. The forward-advance detection this plan hooks into for `last_known_good_offset_hz`: the same
   block above already computes `dx_frame_advanced` and had `progress.metadata.dx_repeat_count =
   0;` on that branch (~line 3005) — that's where `last_known_good_offset_hz` gets updated
   instead.
3. `rearm_manual_calls_at` (~lines 5116-5296) — the per-~15s-slot re-send loop. This is the new
   sole increment site for `stall_cycles`.

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs` (all three spots above)
- Test: `pancetta-qso/src/qso_manager.rs` inline test module

**Interfaces:**
- Consumes: `TimeoutConfig::qso_stall_switch_after` (Task 2), `OffsetAction`/
  `QsoEvent::TxOffsetActionNeeded` (Task 3)
- Produces: the actual stall→switch/revert behavior. Task 5's `apply_tx_offset_switch` consumes
  `stall_cycles`/`last_known_good_offset_hz` the same way this task reads/writes them.

- [ ] **Step 1: Write the failing tests**

Follow the existing test-construction helpers in this file (grep for a helper that builds a
`QsoManager` + drives it into `RespondingToCq` or `SendingReport` via `respond_to_cq`/
`respond_to_cq_with` — reuse it rather than hand-rolling QSO setup) and write:

```rust
#[tokio::test]
async fn silence_increments_stall_cycles_via_rearm_and_emits_switch_at_threshold() {
    // Arrange: a QSO in an Auto, tx_freq_mode=Auto, rearm-eligible state
    // (RespondingToCq or SendingReport — use whichever the file's existing
    // helper produces) with config.timeouts.qso_stall_switch_after = 2 for a
    // fast test.
    let mut config = QsoManagerConfig::default();
    config.timeouts.qso_stall_switch_after = 2;
    let manager = QsoManager::new(config);
    manager.set_tx_freq_mode_source(Arc::new(std::sync::atomic::AtomicU8::new(
        pancetta_core::TxFreqMode::Auto.as_u8(),
    )));
    let mut rx = manager.subscribe();
    // ... drive the manager into a rearm-eligible Auto QSO here, mirroring
    // whatever existing test in this file already does this for the
    // AUTO_RESEND_MAX_CALLS-adjacent tests (search for
    // "RespondingToCq" and "CallInitiation::Auto" together) ...
    let qso_id = /* the QSO id from that setup */;
    let start = chrono::Utc::now();
    // First rearm tick: SLOT_SECONDS after the initial call, no DX response.
    manager.rearm_manual_calls_at(start + chrono::Duration::seconds(16)).await;
    // Second rearm tick: threshold (2) hit -> should emit TxOffsetActionNeeded::Switch.
    manager.rearm_manual_calls_at(start + chrono::Duration::seconds(32)).await;

    let mut saw_switch = false;
    while let Ok(event) = rx.try_recv() {
        if let QsoEvent::TxOffsetActionNeeded {
            qso_id: id,
            action: OffsetAction::Switch { .. },
        } = event
        {
            assert_eq!(id, qso_id);
            saw_switch = true;
        }
    }
    assert!(saw_switch, "expected a Switch action after 2 silent rearm cycles");
}

#[tokio::test]
async fn forward_advance_resets_stall_cycles_and_records_known_good_offset() {
    // Arrange the same way; drive one rearm tick (stall_cycles -> 1), then
    // feed a genuinely advancing DX message via process_message/
    // process_message_with_parity (whatever the file's existing "DX
    // advances the QSO" tests use), then assert via get_qso(qso_id) that
    // metadata.stall_cycles == 0 and metadata.last_known_good_offset_hz ==
    // Some(metadata.frequency).
}

#[tokio::test]
async fn second_stall_on_switched_offset_reverts_to_known_good() {
    // Arrange a QSO that has already advanced once (so
    // last_known_good_offset_hz is Some(X)), then externally move
    // metadata.frequency away from X (simulating a completed first Switch —
    // this test can call the manager's internal test-only mutation if one
    // exists, or more realistically: drive TWO real stall cycles and assert
    // the SECOND TxOffsetActionNeeded emitted is a Revert{target_hz: X},
    // not another Switch). Assert the second emitted event is
    // OffsetAction::Revert { target_hz } where target_hz equals the
    // known-good offset from the first advance.
}

#[tokio::test]
async fn hold_mode_never_emits_tx_offset_action_needed() {
    // Same setup but tx_freq_mode = Hold. Drive well past
    // qso_stall_switch_after silent rearm cycles. Assert no
    // TxOffsetActionNeeded event is ever emitted (mirrors the existing
    // "no hop in Hold mode" test for the old dx_repeat_count mechanism —
    // find it and model this one on it exactly, then delete the old one
    // per Task 1 Step 5's note).
}
```

Fill in the setup helpers by reading the existing tests in this file for the closest analog (the
old `DX_STUCK_REPEAT_THRESHOLD`/`stuck_hopped_offset` tests you're about to delete are the best
template — they already build exactly this kind of Auto, rearm-eligible QSO). Do not guess at
helper names; copy the real setup from a real existing test.

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso silence_increments_stall_cycles`
Expected: FAIL (behavior not implemented yet — either a compile error if you reference
not-yet-existing internals, or an assertion failure).

- [ ] **Step 3: Remove the old mechanism**

Delete `DX_STUCK_REPEAT_THRESHOLD`, `STUCK_TX_HOP_HZ`, and `stuck_hopped_offset` (qso_manager.rs
~lines 19-93 — keep `TX_OFFSET_MIN_HZ`/`TX_OFFSET_MAX_HZ`/the Hound region constants, only the
three stuck-hop-specific items go). Delete the "Stuck-DX TX-frequency hold/escape" block inside
`process_message_for_qso` (~lines 2985-3033) — the whole comment block plus the `if let
Some(progress) = qsos.get_mut(&qso_id) { if dx_frame_advanced { ... } ... if tx_auto && ... {
...hop... } }` body — but do NOT delete the `let tx_auto = ...` line (~2998-3001) or the `let
rx_text = ...` line (~3002) — both are still needed by Step 4 below.

- [ ] **Step 4: Implement forward-advance tracking**

Where the deleted block used to reset `dx_repeat_count`/`last_rx_text` on advance, add (using the
same `if let Some(progress) = qsos.get_mut(&qso_id)` scope that was already there):

```rust
        if let Some(progress) = qsos.get_mut(&qso_id) {
            if dx_frame_advanced {
                progress.metadata.stall_cycles = 0;
                progress.metadata.last_known_good_offset_hz = Some(progress.metadata.frequency);
            }
        }
```

(This replaces the entire deleted block's body — the repeated-frame/`dx_repeat_count`-specific
branches are gone; `stall_cycles` for the non-advancing case is now driven solely by
`rearm_manual_calls_at`, not by incoming-message inspection, so there is no "else" branch here
anymore.)

- [ ] **Step 5: Implement stall-cycle increment + threshold emission in `rearm_manual_calls_at`**

In `rearm_manual_calls_at` (~lines 5116-5296), immediately after the existing
`progress.metadata.call_count += 1; progress.metadata.last_call_at = Some(now);` (~lines
5254-5255, inside the per-QSO loop, after the `max_calls`/`elapsed_since_last` gates have already
passed — i.e. this is a real re-send, not a skip), add:

```rust
                progress.metadata.stall_cycles = progress.metadata.stall_cycles.saturating_add(1);

                let tx_auto = pancetta_core::TxFreqMode::from_u8(
                    self.tx_freq_mode.load(std::sync::atomic::Ordering::Relaxed),
                )
                .allows_auto_change();

                if tx_auto
                    && progress.metadata.stall_cycles >= self.config.timeouts.qso_stall_switch_after
                {
                    let current = progress.metadata.frequency;
                    let action = match progress.metadata.last_known_good_offset_hz {
                        Some(known_good) if (current - known_good).abs() >= f64::EPSILON => {
                            OffsetAction::Revert { target_hz: known_good }
                        }
                        _ => OffsetAction::Switch { avoid_hz: current },
                    };
                    progress.metadata.stall_cycles = 0;
                    offset_actions_to_emit.push((qso_id, action));
                }
```

This needs a `let mut offset_actions_to_emit: Vec<(QsoId, OffsetAction)> = Vec::new();` declared
before the per-QSO `for` loop starts (alongside the existing `let mut to_recall = Vec::new();`
declaration, ~line 5136-5142 — same reason: the `qsos` write-lock must be dropped before any
`.await`, so collect first, emit after), and after the lock-scoped block ends (alongside the
existing `for (qso_id, message, frequency, tx_parity, remote_origin) in to_recall { ...
self.emit_event(...).await; }` loop, ~lines 5282-5295), add:

```rust
        for (qso_id, action) in offset_actions_to_emit {
            self.emit_event(QsoEvent::TxOffsetActionNeeded { qso_id, action }).await;
        }
```

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso --features transmit`
Expected: PASS, including the four new tests from Step 1 and everything from Task 1's Step 5
(the old dx_repeat_count-mechanism tests you deleted/adapted there).

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "$(cat <<'EOF'
feat(qso): generalize mid-QSO stuck detection to silence, emit TxOffsetActionNeeded

Replaces the fixed +300Hz repeat-frame hop with a unified stall_cycles
counter (silence or repeat both count) that emits a Switch or Revert
action for the coordinator to resolve via the smart allocator (PAN-72).
EOF
)"
```

---

### Task 5: `QsoManager::apply_tx_offset_switch`

**Files:**
- Modify: `pancetta-qso/src/qso_manager.rs` (new method, place it near `resend_last_tx`,
  ~line 2552, which is the closest existing template — same `&self` + `qsos.write().await` +
  `QsoManagerError::QsoNotFound` pattern)
- Test: `pancetta-qso/src/qso_manager.rs` inline test module

**Interfaces:**
- Consumes: nothing new from earlier tasks besides `QsoManagerError` (pre-existing)
- Produces: `pub async fn apply_tx_offset_switch(&self, qso_id: QsoId, new_offset_hz: f64) ->
  Result<(), QsoManagerError>` — Task 9 (coordinator) is the caller.

- [ ] **Step 1: Write the failing test**

```rust
#[tokio::test]
async fn apply_tx_offset_switch_updates_frequency_and_resets_stall_cycles() {
    let config = QsoManagerConfig::default();
    let manager = QsoManager::new(config);
    // ... drive the manager to create a QSO (reuse an existing helper), get its qso_id ...
    // Manually bump stall_cycles first via whatever the file's test helpers expose, or
    // via a real stalled sequence, to prove the reset actually happens.
    manager.apply_tx_offset_switch(qso_id, 1800.0).await.unwrap();
    let (_, progress) = manager
        .get_active_qsos()
        .await
        .into_iter()
        .find(|(id, _)| *id == qso_id)
        .unwrap();
    assert_eq!(progress.metadata.frequency, 1800.0);
    assert_eq!(progress.metadata.stall_cycles, 0);
}

#[tokio::test]
async fn apply_tx_offset_switch_on_unknown_qso_returns_not_found() {
    let manager = QsoManager::new(QsoManagerConfig::default());
    let result = manager.apply_tx_offset_switch(QsoId::new_v4(), 1800.0).await;
    assert!(matches!(result, Err(QsoManagerError::QsoNotFound { .. })));
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso apply_tx_offset_switch`
Expected: FAIL — `no method named \`apply_tx_offset_switch\``.

- [ ] **Step 3: Implement the method**

```rust
    /// External commit point for PAN-72's stall-switch/revert and the manual
    /// nudge keystroke — the only way outside code may change an
    /// already-active QSO's TX offset. Mirrors the mutation the removed
    /// stuck-DX hop used to perform inline; the caller (the coordinator) has
    /// already resolved WHAT the new offset should be (via the smart
    /// allocator for a Switch, or `last_known_good_offset_hz` for a Revert —
    /// `QsoManager` itself never calls the allocator, see the single-scorer
    /// invariant). Does not force an immediate retransmission — the next
    /// naturally-scheduled send picks up the new value since message
    /// construction reads `metadata.frequency` fresh each time.
    pub async fn apply_tx_offset_switch(
        &self,
        qso_id: QsoId,
        new_offset_hz: f64,
    ) -> Result<(), QsoManagerError> {
        let mut qsos = self.qsos.write().await;
        let progress = qsos
            .get_mut(&qso_id)
            .ok_or(QsoManagerError::QsoNotFound { qso_id })?;
        let old_off = progress.metadata.frequency;
        progress.metadata.frequency = new_offset_hz;
        progress.metadata.pending_freq_drift = None;
        progress.metadata.stall_cycles = 0;
        warn!(
            target: "tx.freq",
            qso_id = %qso_id,
            dx = progress.metadata.their_callsign.as_deref().unwrap_or("?"),
            "Adaptive TX-offset action: {:.0} Hz -> {:.0} Hz",
            old_off, new_offset_hz
        );
        Ok(())
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso apply_tx_offset_switch`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add pancetta-qso/src/qso_manager.rs
git commit -m "feat(qso): add QsoManager::apply_tx_offset_switch external mutation entry point"
```

---

### Task 6: `pancetta-config` — `qso_stall_switch_after` and lowered `cq_no_response_switch_after`

**Files:**
- Modify: `pancetta-config/src/autonomous.rs` (struct fields ~196-260, `validate_section`
  ~263-302, `merge_with` ~304-319, and the default fn area ~192-194)
- Modify: `pancetta-qso/src/autonomous.rs` (the runtime `AutonomousConfig` struct fields ~528-535
  and its `Default` impl ~566-567) — **this is a different `AutonomousConfig` type in a different
  crate**; do not confuse the two.
- Modify: `docs/CONFIG.md`
- Test: both crates' existing config test modules

**Interfaces:**
- Consumes: nothing
- Produces: `pancetta_config::AutonomousConfig::qso_stall_switch_after: u32` (default 4, TOML
  `[autonomous]` section), `pancetta_config::AutonomousConfig::cq_no_response_switch_after`
  default now 4, `pancetta_qso::autonomous::AutonomousConfig::cq_no_response_switch_after`
  default now 4. Task 7 reads `pancetta_config`'s `qso_stall_switch_after` and threads it into
  `pancetta_qso::qso_manager::TimeoutConfig::qso_stall_switch_after` (Task 2's field) — these are
  two different `qso_stall_switch_after` fields on two different structs in two different
  crates, connected only by the coordinator's threading code in Task 7. Do NOT add
  `qso_stall_switch_after` to `pancetta_qso::autonomous::AutonomousConfig` — it belongs on
  `pancetta_qso::qso_manager::TimeoutConfig` (Task 2), which is a different struct again.

- [ ] **Step 1: Write the failing tests**

In `pancetta-config/src/autonomous.rs`'s test module:

```rust
#[test]
fn default_qso_stall_switch_after_is_4() {
    let config = AutonomousConfig::default();
    assert_eq!(config.qso_stall_switch_after, 4);
}

#[test]
fn config_missing_qso_stall_switch_after_field_uses_default() {
    // Mirror config_missing_cq_no_response_switch_after_field_uses_default
    // (~line 400-444 in this file) exactly, minus the one field under test —
    // parse a TOML fragment for [autonomous] that omits
    // qso_stall_switch_after and assert it deserializes to 4.
}
```

Update the existing test asserting the old CQ default:

```rust
#[test]
fn autonomous_config_default_cq_no_response_switch_after_is_5() {
    // RENAME to _is_4, update the assertion:
    let config = AutonomousConfig::default();
    assert_eq!(config.cq_no_response_switch_after, 4);
}
```

In `pancetta-qso/src/autonomous.rs`'s test module, find and update the equivalent default-value
assertion for its own `AutonomousConfig::default().cq_no_response_switch_after` the same way (it
will be a distinct test in this crate — search for `cq_no_response_switch_after, 5` in this file
specifically, do not assume it's named identically to the `pancetta-config` one).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-config qso_stall_switch_after`
Run: `cargo test -p pancetta-config autonomous_config_default_cq_no_response_switch_after`
Run: `cargo test -p pancetta-qso cq_no_response_switch_after` (find the exact test name in that
crate first via `grep -rn "cq_no_response_switch_after, 5" pancetta-qso/src/autonomous.rs`)
Expected: all FAIL (missing field / wrong assertion value).

- [ ] **Step 3: `pancetta-config/src/autonomous.rs` changes**

Add near `default_cq_no_response_switch_after` (~line 192-194):

```rust
fn default_qso_stall_switch_after() -> u32 {
    4
}
```

Change:
```rust
fn default_cq_no_response_switch_after() -> u32 {
    5
}
```
to:
```rust
fn default_cq_no_response_switch_after() -> u32 {
    4
}
```

Add to the `AutonomousConfig` struct (right after `cq_no_response_switch_after`, ~line 202):

```rust
    /// Consecutive stalled cycles before an in-progress QSO's TX offset is
    /// switched/reverted (Auto TX-freq mode only). Threaded by the
    /// coordinator into `pancetta_qso::QsoManagerConfig::TimeoutConfig::
    /// qso_stall_switch_after` — see that field's doc comment for the full
    /// switch/revert mechanics (PAN-72). Default 4.
    #[serde(default = "default_qso_stall_switch_after")]
    pub qso_stall_switch_after: u32,
```

Add to `merge_with` (~line 304-319, alongside the existing `cq_no_response_switch_after` line):

```rust
        self.qso_stall_switch_after = other.qso_stall_switch_after;
```

Add to `validate_section` (~line 263-302, mirroring the existing `cq_no_response_switch_after ==
0` check at ~270-275):

```rust
        if self.qso_stall_switch_after == 0 {
            return Err(ConfigError::InvalidValue {
                field: "autonomous.qso_stall_switch_after".to_string(),
                reason: "must be at least 1".to_string(),
            });
        }
```

(Use whichever exact `ConfigError` variant/construction the existing `cq_no_response_switch_after
== 0` check uses — copy its exact shape rather than the sketch above if it differs.)

- [ ] **Step 4: `pancetta-qso/src/autonomous.rs` change**

Change the `Default for AutonomousConfig` impl's `cq_no_response_switch_after: 5,` (~line 567) to
`cq_no_response_switch_after: 4,`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p pancetta-config` and `cargo test -p pancetta-qso`
Expected: PASS, including the `assert_carries_all::<autonomous::AutonomousConfig>` guardrail
test in `pancetta-config/src/lib.rs` (it's type-driven, so it should pick up the new field
automatically as long as Step 3's `merge_with` line is present — run
`cargo test -p pancetta-config assert_carries_all` explicitly to confirm).

- [ ] **Step 6: Update `docs/CONFIG.md`**

Find the `[autonomous]` section's documentation and add `qso_stall_switch_after` (default 4,
"consecutive stalled mid-QSO cycles before switching/reverting the QSO's TX offset — Auto mode
only") next to the existing `cq_no_response_switch_after` entry (update that entry's stated
default from 5 to 4 too).

- [ ] **Step 7: Commit**

```bash
git add pancetta-config/ pancetta-qso/src/autonomous.rs docs/CONFIG.md
git commit -m "$(cat <<'EOF'
feat(config): add autonomous.qso_stall_switch_after, lower cq_no_response_switch_after to 4

PAN-72: both are now operator-tunable under [autonomous] with a shared
"how many failed cycles before trying something else" theme, even though
they land in different Rust types internally (QsoManagerConfig vs
AutonomousConfig — see the field doc comments).
EOF
)"
```

---

### Task 7: Coordinator threads `qso_stall_switch_after` into `QsoManagerConfig`

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs:2417-2441` (the `QsoManagerConfig { ... }`
  construction site)

**Interfaces:**
- Consumes: `self.config.autonomous.qso_stall_switch_after` (Task 6),
  `pancetta_qso::qso_manager::TimeoutConfig::qso_stall_switch_after` (Task 2)
- Produces: a live `QsoManager` whose `config.timeouts.qso_stall_switch_after` reflects the
  operator's TOML setting instead of the Rust default.

- [ ] **Step 1: Change the construction site**

In `pancetta/src/coordinator/qso.rs`, the `qso_config` block currently does `..Default::default()`
for the whole `QsoManagerConfig` after setting `our_callsign`/`our_grid`/`hound`/`active_mode`/
`duplicate_checking` explicitly (~lines 2420-2440). Add an explicit `timeouts` field:

```rust
                timeouts: pancetta_qso::qso_manager::TimeoutConfig {
                    qso_stall_switch_after: self.config.autonomous.qso_stall_switch_after,
                    ..Default::default()
                },
```

placed among the other explicit fields in that literal (before the trailing `..Default::default()`
for the rest of `QsoManagerConfig`). Confirm `TimeoutConfig` is `pub` and its non-`qso_stall_
switch_after` fields all implement `Default` sanely via `TimeoutConfig::default()` (Task 2 didn't
change this) so the `..Default::default()` spread inside this nested literal is valid.

- [ ] **Step 2: Build and run the coordinator's existing test suite**

Run: `cargo build -p pancetta`
Run: `cargo test -p pancetta --features transmit qso`
Expected: both succeed with no behavior change to any existing test (this task only changes what
value one previously-defaulted field gets, and only when `self.config.autonomous.
qso_stall_switch_after` differs from `TimeoutConfig::default()`'s value — both are 4 after Task 6,
so no existing test should observe a difference).

- [ ] **Step 3: Write a targeted test proving the thread-through**

Find how existing tests in `pancetta/src/coordinator/qso.rs` construct a coordinator (or a
`QsoManagerConfig`) from a `pancetta_config::Config` with non-default values, and add one that
sets `config.autonomous.qso_stall_switch_after = 7` in a `pancetta_config::Config`, builds
whatever the existing test-construction helper for this file produces, and asserts the resulting
`QsoManagerConfig.timeouts.qso_stall_switch_after == 7`. If no such coordinator-config-threading
test convention exists anywhere in this file (some coordinator wiring may only be covered by
manual/integration testing), do not invent new test-harness machinery for this alone — say so in
the commit message and rely on Step 2's build+existing-suite pass plus Task 12's full workspace
run instead.

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/coordinator/qso.rs
git commit -m "feat(coordinator): thread autonomous.qso_stall_switch_after into QsoManagerConfig"
```

---

### Task 8: Coordinator mailbox — `pending_qso_offset_requests`

**Files:**
- Modify: `pancetta/src/coordinator/mod.rs` (new field, mirrors
  `pending_autonomous_cq_dispatch_failures` at line 677 and its initializer at line 1851)
- Modify: `pancetta/src/coordinator/qso.rs` (push site, mirrors how `pending_autonomous_cq_
  dispatch_failures` is cloned into the event-forwarding task at ~lines 2266-2267, and the new
  match arm goes in the same `match qso_events.recv().await { ... }` block that already handles
  `QsoEvent::StateChanged`/`QsoEvent::MessageToSend`/etc., starting ~line 2812)
- Test: wherever this file's event-forwarding task already has tests (search for a test that
  sends a `QsoEvent::StateChanged` through a manager and asserts `active_tx_qsos` got updated —
  mirror it)

**Interfaces:**
- Consumes: `QsoEvent::TxOffsetActionNeeded` (Task 3)
- Produces: `ApplicationCoordinator::pending_qso_offset_requests: Arc<std::sync::Mutex<Vec<(pancetta_qso::states::QsoId, pancetta_qso::qso_manager::OffsetAction)>>>`
  — Task 9 drains this.

- [ ] **Step 1: Add the field to `mod.rs`**

Alongside `pending_autonomous_cq_dispatch_failures` (line 677):

```rust
    /// PAN-72: mailbox of resolved-needed TX-offset actions for in-progress
    /// QSOs, pushed by the QSO event-forwarding task (`coordinator/qso.rs`)
    /// on `QsoEvent::TxOffsetActionNeeded`, drained once per 15s slot by the
    /// Autonomous task (`coordinator/autonomous.rs`) — same
    /// push-mailbox/drain-once-per-tick shape as
    /// `pending_autonomous_cq_dispatch_failures` above.
    pending_qso_offset_requests: Arc<
        std::sync::Mutex<Vec<(pancetta_qso::states::QsoId, pancetta_qso::qso_manager::OffsetAction)>>,
    >,
```

And its initializer alongside line 1851:

```rust
            pending_qso_offset_requests: Arc::new(std::sync::Mutex::new(Vec::new())),
```

- [ ] **Step 2: Write the failing test**

Find the existing test that drives a `QsoEvent::StateChanged` through the coordinator's
event-forwarding task and asserts `active_tx_qsos` updates (grep this file's test module for
`active_tx_qsos` inside a `#[tokio::test]`). Copy its setup and add:

```rust
// (naming/setup mirrors the existing active_tx_qsos-update test in this
// file — adapt field names to match whatever that test actually calls its
// coordinator/harness variable)
#[tokio::test]
async fn tx_offset_action_needed_event_is_forwarded_to_pending_qso_offset_requests() {
    // ... reuse the existing harness setup ...
    qso_event_sender
        .send(pancetta_qso::QsoEvent::TxOffsetActionNeeded {
            qso_id,
            action: pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz: 1500.0 },
        })
        .unwrap();
    // give the forwarding task a tick to process, same as the existing test does
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let pending = coordinator.pending_qso_offset_requests.lock().unwrap();
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].0, qso_id);
}
```

- [ ] **Step 3: Run test to verify it fails**

Run: `cargo test -p pancetta tx_offset_action_needed_event_is_forwarded`
Expected: FAIL — either a compile error (field doesn't exist until Step 1, so do Step 1 first
then this becomes a runtime assertion failure) or `pending.len() == 0`.

- [ ] **Step 4: Add the push site**

In `pancetta/src/coordinator/qso.rs`, clone the new field into the spawned task alongside the
existing clones (~lines 2795-2799):

```rust
                let pending_qso_offset_requests = self.pending_qso_offset_requests.clone();
```

Add a new match arm in the `match qso_events.recv().await { ... }` block (alongside the existing
`Ok(pancetta_qso::QsoEvent::StateChanged { ... }) => { ... }` arm, ~line 2813):

```rust
                            Ok(pancetta_qso::QsoEvent::TxOffsetActionNeeded { qso_id, action }) => {
                                if let Ok(mut pending) = pending_qso_offset_requests.lock() {
                                    pending.push((qso_id, action));
                                }
                            }
```

- [ ] **Step 5: Run test to verify it passes**

Run: `cargo test -p pancetta tx_offset_action_needed_event_is_forwarded`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/mod.rs pancetta/src/coordinator/qso.rs
git commit -m "feat(coordinator): add pending_qso_offset_requests mailbox, forward TxOffsetActionNeeded"
```

---

### Task 9: Autonomous task drains the mailbox and commits via the allocator

**Files:**
- Modify: `pancetta-qso/src/autonomous.rs` (bump `allocate_smart_frequency` from private to
  `pub(crate)`, ~line 1735-1740 — no signature change)
- Modify: `pancetta/src/coordinator/autonomous.rs` (new drain function mirroring
  `drain_pending_autonomous_cq_dispatch_failures` at ~lines 512-528, call site mirroring ~lines
  1220-1223, and the field clone mirroring ~lines 1110-1111)
- Test: `pancetta/src/coordinator/autonomous.rs`'s existing
  `drain_pending_autonomous_cq_dispatch_failures_tests` module (~line 2874) — add a sibling
  module for this drain fn following the exact same structure

**Interfaces:**
- Consumes: `pending_qso_offset_requests` (Task 8), `OffsetAction` (Task 3),
  `apply_tx_offset_switch` (Task 5), `allocate_smart_frequency` (now `pub`, crate-external —
  `pancetta` and `pancetta-qso` are separate crates, so `pub(crate)` would not be visible to the
  coordinator)
- Produces: the actual commit-to-QsoManager behavior. Nothing later depends on new interfaces
  from this task beyond it working correctly.

- [ ] **Step 1: Bump `allocate_smart_frequency` visibility to `pub`**

In `pancetta-qso/src/autonomous.rs` (~line 1735), change:
```rust
    fn allocate_smart_frequency(
```
to (adding a doc comment explaining why it's now exposed outside the crate; no signature change):
```rust
    /// Exposed (beyond this crate's own internal use in `decide_at`) for the
    /// coordinator's PAN-72 mid-QSO stall-switch drain
    /// (`pancetta::coordinator::autonomous`), which resolves an
    /// `OffsetAction::Switch` the same way a CQ-hunting switch does — see
    /// that call site's own doc comment. No signature change from the
    /// pre-existing private method.
    pub fn allocate_smart_frequency(
```

Run: `cargo build -p pancetta-qso` to confirm this compiles (a pure visibility widening).

- [ ] **Step 2: Write the failing test**

In `pancetta/src/coordinator/autonomous.rs`, add a new test module right after
`drain_pending_autonomous_cq_dispatch_failures_tests` (~line 2874), copying that module's
`AutonomousOperator`/mock-manager construction pattern:

```rust
#[cfg(test)]
mod drain_pending_qso_offset_requests_tests {
    use super::*;

    #[tokio::test]
    async fn switch_action_resolves_via_allocator_and_commits() {
        // Mirror this file's existing AutonomousOperator + QsoManager
        // construction from drain_pending_autonomous_cq_dispatch_failures_tests.
        let mut op = /* ... */;
        let qso_manager = /* a QsoManager with one active QSO at 1500.0 Hz */;
        let qso_id = /* that QSO's id */;
        let pending = std::sync::Mutex::new(vec![(
            qso_id,
            pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz: 1500.0 },
        )]);

        drain_pending_qso_offset_requests(&mut op, &qso_manager, &pending).await;

        assert!(pending.lock().unwrap().is_empty());
        let (_, progress) = qso_manager
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(id, _)| *id == qso_id)
            .unwrap();
        assert!((progress.metadata.frequency - 1500.0).abs() > f64::EPSILON);
    }

    #[tokio::test]
    async fn revert_action_uses_target_hz_directly_no_allocator_call() {
        // Same shape, OffsetAction::Revert { target_hz: 1200.0 }; assert the
        // QSO's frequency becomes exactly 1200.0 (not allocator output).
    }
}
```

- [ ] **Step 3: Run tests to verify they fail**

Run: `cargo test -p pancetta drain_pending_qso_offset_requests`
Expected: FAIL — `cannot find function \`drain_pending_qso_offset_requests\``.

- [ ] **Step 4: Implement the drain function**

Mirror `drain_pending_autonomous_cq_dispatch_failures` (~lines 512-528) exactly in shape:

```rust
/// PAN-72: resolves and commits every queued mid-QSO TX-offset action once
/// per tick, mirroring `drain_pending_autonomous_cq_dispatch_failures`'s
/// mailbox shape. `Switch` is resolved via the SAME `allocate_smart_frequency`
/// the CQ-hunting switch uses (single-scorer invariant); `Revert` needs no
/// allocator call. Errors from `apply_tx_offset_switch` (e.g. the QSO
/// completed/was removed between the event firing and this drain) are logged
/// and skipped, not propagated — a stale request for a QSO that no longer
/// exists is not this task's problem to recover from.
async fn drain_pending_qso_offset_requests(
    op: &mut pancetta_qso::AutonomousOperator,
    qso_manager: &pancetta_qso::QsoManager,
    pending_qso_offset_requests: &std::sync::Mutex<
        Vec<(pancetta_qso::states::QsoId, pancetta_qso::qso_manager::OffsetAction)>,
    >,
) {
    let requests: Vec<_> = std::mem::take(
        &mut *pending_qso_offset_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    for (qso_id, action) in requests {
        let resolved_hz = match action {
            pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz } => {
                op.allocate_smart_frequency(None, None, Some(avoid_hz))
            }
            pancetta_qso::qso_manager::OffsetAction::Revert { target_hz } => target_hz,
        };
        if let Err(err) = qso_manager
            .apply_tx_offset_switch(qso_id, resolved_hz)
            .await
        {
            tracing::warn!(
                target: "tx.freq",
                qso_id = %qso_id,
                error = %err,
                "PAN-72: could not apply queued TX-offset action (QSO likely completed)"
            );
        }
    }
}
```

- [ ] **Step 5: Wire the drain into the tick loop**

In the Autonomous task's spawn setup (~line 1110-1111, alongside the existing `pending_
autonomous_cq_dispatch_failures` clone), add:

```rust
        let pending_qso_offset_requests = self.pending_qso_offset_requests.clone();
```

and confirm a `qso_manager` handle is already reachable inside this spawned closure (check
whether `self.qso_manager_for_supervisor` — seen referenced in `coordinator/qso.rs` — or some
other existing clone is already captured here; if the Autonomous task doesn't currently hold any
`QsoManager` handle, clone `self.qso_manager_for_supervisor` the same way other `Arc`-cloned
handles are captured at spawn time, immediately before `tokio::spawn` at ~line 1117).

In the tick arm, alongside the existing `drain_pending_autonomous_cq_dispatch_failures(...)` call
(~lines 1220-1223), add:

```rust
                            drain_pending_qso_offset_requests(
                                &mut op,
                                &qso_manager,
                                &pending_qso_offset_requests,
                            )
                            .await;
```

(`op` is already locked at this point in the existing code, per the surrounding
`operator.lock().await` pattern — confirm this new call sits inside that same locked scope,
matching where `drain_pending_autonomous_cq_dispatch_failures` already sits.)

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p pancetta drain_pending_qso_offset_requests`
Run: `cargo build -p pancetta` (confirm the tick-loop wiring compiles)
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
git add pancetta-qso/src/autonomous.rs pancetta/src/coordinator/autonomous.rs
git commit -m "$(cat <<'EOF'
feat(coordinator): drain pending_qso_offset_requests each tick, resolve via allocator

PAN-72: Switch resolves through AutonomousOperator::allocate_smart_frequency
(now pub, still the sole allocator caller); Revert uses the recorded
known-good offset directly. Commits via QsoManager::apply_tx_offset_switch.
EOF
)"
```

---

### Task 10: Manual `u` keystroke — `TuiCommand::NudgeTxOffset`

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs` (new `TuiCommand` variant ~line 328-526, new key match
  arm near the existing `t`/`a` arms ~lines 1923-1979)
- Modify: `pancetta/src/coordinator/mod.rs` (new `pending_cq_offset_nudge: Arc<AtomicBool>` field,
  alongside `pending_qso_offset_requests`)
- Modify: `pancetta/src/coordinator/tui_relay.rs` (new match arm for `TuiCommand::NudgeTxOffset`,
  near the existing `SetTxOffset` arm ~line 1802)
- Modify: `pancetta-qso/src/autonomous.rs` (new `manual_switch_requested: bool` field on
  `AutonomousOperator`, a `pub fn request_manual_switch(&mut self)` setter, and one line in
  `decide_at`'s `should_switch` computation ~line 2431-2433)
- Modify: `pancetta/src/coordinator/autonomous.rs` (drain the new flag into
  `op.request_manual_switch()` each tick, alongside the other pre-`decide()` pushes)
- Test: `pancetta-tui/src/tui_runner.rs` (keybinding dispatch test, mirroring an existing one for
  `t` or `a`), `pancetta/src/coordinator/tui_relay.rs` (dispatch test, mirroring the existing
  `SetTxOffset`/`ToggleAutonomous` ones), `pancetta-qso/src/autonomous.rs` (unit test for
  `request_manual_switch` forcing a switch)

**Interfaces:**
- Consumes: `pending_qso_offset_requests` (Task 8, for the "active QSO" branch),
  `active_tx_qsos` (pre-existing coordinator snapshot)
- Produces: the manual-nudge behavior end to end. Nothing later depends on this task.

- [ ] **Step 1: `AutonomousOperator::request_manual_switch` — write the failing test**

In `pancetta-qso/src/autonomous.rs`'s test module, find an existing test that drives `decide_at`
through a `CallingCq` state with `cq_no_response_streak` below `cq_no_response_switch_after` and
asserts NO switch happens (a "routine CQ, streak not yet met" test) — copy its setup, then:

```rust
#[test]
fn request_manual_switch_forces_a_switch_below_the_streak_threshold() {
    let mut op = /* same setup as the routine-CQ test above, streak = 0 */;
    op.request_manual_switch();
    let actions = op.decide_at(/* same fixed time the copied test uses */);
    assert!(actions
        .iter()
        .any(|a| matches!(a, OperatorAction::FrequencyShift { .. })));
}

#[test]
fn request_manual_switch_is_consumed_even_when_not_calling_cq() {
    let mut op = /* an operator NOT in CallingCq state — e.g. Hunting */;
    op.request_manual_switch();
    let _ = op.decide_at(/* any fixed time */);
    // Second decide_at call must NOT retroactively force a switch once the
    // operator later enters CallingCq on its own — the flag must not leak
    // across an unrelated cycle. Drive a second, later decide_at that WOULD
    // enter CallingCq via the routine idle-cycles path, and assert it does
    // NOT force a switch this time (no lingering manual_switch_requested).
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-qso request_manual_switch`
Expected: FAIL — `no method named \`request_manual_switch\``.

- [ ] **Step 3: Implement `request_manual_switch` and wire it into `decide_at`**

Add a field to `AutonomousOperator`'s struct (alongside `cq_no_response_streak`, ~line 884):

```rust
    /// PAN-72: set by `request_manual_switch` (the TUI's `u` "nudge"
    /// keystroke, relayed via the coordinator), consumed unconditionally at
    /// the top of `decide_at` regardless of the current operating state — so
    /// a nudge that arrives while not `CallingCq` doesn't leak into a LATER,
    /// unrelated CQ cycle.
    manual_switch_requested: bool,
```

Initialize it `false` in `AutonomousOperator::new` (alongside `cq_no_response_streak: 0,` ~line
1052).

Add the setter, near other small `pub fn` setters on this type:

```rust
    /// PAN-72: request an immediate CQ-offset switch on the next `decide_at`
    /// call, bypassing `cq_no_response_switch_after` — used by the manual
    /// `u` keystroke. A no-op if the next cycle isn't `CallingCq` (the flag
    /// is still consumed, not left pending for some later cycle).
    pub fn request_manual_switch(&mut self) {
        self.manual_switch_requested = true;
    }
```

At the very top of `decide_at` (before any state-dependent branching — check the method's exact
first lines and insert right after entry), add:

```rust
        let manual_switch_requested = std::mem::take(&mut self.manual_switch_requested);
```

Change the `should_switch` computation (~line 2431-2433) from:
```rust
                            let should_switch = self.tx_freq_auto()
                                && self.cq_no_response_streak
                                    >= self.config.cq_no_response_switch_after;
```
to:
```rust
                            let should_switch = self.tx_freq_auto()
                                && (self.cq_no_response_streak
                                    >= self.config.cq_no_response_switch_after
                                    || manual_switch_requested);
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-qso request_manual_switch`
Expected: PASS. Also run `cargo test -p pancetta-qso --features transmit` fully to confirm the
`should_switch` change didn't disturb any of the many existing CQ-switch tests referenced in the
research (PR #276-#277, PAN-38/PAN-44 rounds) — it shouldn't, since `manual_switch_requested` is
`false` in every existing test that never calls `request_manual_switch`.

- [ ] **Step 5: Commit (qso crate half)**

```bash
git add pancetta-qso/src/autonomous.rs
git commit -m "feat(qso): add AutonomousOperator::request_manual_switch for the manual nudge keystroke"
```

- [ ] **Step 6: Add `TuiCommand::NudgeTxOffset` and the `u` keybinding**

In `pancetta-tui/src/tui_runner.rs`'s `TuiCommand` enum (near `SetTxOffset`, ~line 490+):

```rust
    /// Operator pressed `u`: force an immediate TX-offset nudge (PAN-72) on
    /// whatever is currently active — an in-progress QSO, or the CQ-hunting
    /// offset if none — without leaving Auto TX-freq mode. No fields: unlike
    /// `t`, the target isn't known client-side (the coordinator decides
    /// based on `active_tx_qsos`), so there's no local optimistic state to
    /// flip.
    NudgeTxOffset,
```

Add the key handler near the existing `t`/`a` arms (~after line 1962, before `// === Autonomous
controls ===`):

```rust
            KeyCode::Char('u') => {
                // PAN-72: "un-stick" — force a nudge without leaving Auto.
                // The coordinator resolves the target (active QSO vs.
                // CQ-hunting) since App doesn't track that state locally.
                self.message_tx.send(TuiCommand::NudgeTxOffset)?;
            }
```

- [ ] **Step 7: Write the dispatch test**

In `tui_runner.rs`'s test module, find an existing test asserting a bare-letter key sends its
`TuiCommand` (e.g. one for `KeyCode::Char('a')` → `TuiCommand::ToggleAutonomous`) and copy its
shape for:

```rust
#[test]
fn u_key_sends_nudge_tx_offset() {
    // mirror the existing `a` -> ToggleAutonomous dispatch test's harness
}
```

- [ ] **Step 8: Run test, verify pass, commit**

Run: `cargo test -p pancetta-tui u_key_sends_nudge_tx_offset`
Expected: PASS.

```bash
git add pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): add 'u' keystroke sending TuiCommand::NudgeTxOffset"
```

- [ ] **Step 9: Coordinator field + relay handling — write the failing test**

Add `pending_cq_offset_nudge: Arc<std::sync::atomic::AtomicBool>` to `ApplicationCoordinator`
(mod.rs, alongside `pending_qso_offset_requests`) and its `false`-initialized constructor line,
same pattern as Task 8 Step 1.

In `pancetta/src/coordinator/tui_relay.rs`'s test module, find the existing `SetTxOffset` or
`ToggleAutonomous` dispatch test and copy its harness for two tests:

```rust
#[tokio::test]
async fn nudge_forces_switch_for_active_qso() {
    // harness with active_tx_qsos containing one entry
    // send TuiCommand::NudgeTxOffset
    // assert pending_qso_offset_requests now has one
    // OffsetAction::Switch{avoid_hz} entry for that qso
}

#[tokio::test]
async fn nudge_sets_cq_flag_when_no_active_qso() {
    // harness with active_tx_qsos empty
    // send TuiCommand::NudgeTxOffset
    // assert pending_cq_offset_nudge.load(Ordering::Relaxed) == true
    // AND pending_qso_offset_requests is still empty
}
```

- [ ] **Step 10: Run tests to verify they fail**

Run: `cargo test -p pancetta nudge_forces_switch_for_active_qso nudge_sets_cq_flag`
Expected: FAIL — `no variant \`NudgeTxOffset\`` (until Step 6 lands, which it already has) or
missing-field compile errors for the new coordinator fields (until this step's Step 11 lands).

- [ ] **Step 11: Implement the relay handling**

In `tui_relay.rs`, add a match arm near `SetTxOffset` (~line 1802):

```rust
                        pancetta_tui::tui_runner::TuiCommand::NudgeTxOffset => {
                            // PAN-72: prefer an active QSO (force a Switch,
                            // bypassing stall_cycles — operator-forced); fall
                            // back to the CQ-hunting offset if none is
                            // active. Does not touch tx_freq_mode/
                            // tx_offset_hold_hz — this must not leave Auto
                            // mode.
                            let active: Vec<String> = active_tx_qsos
                                .read()
                                .map(|s| s.iter().cloned().collect())
                                .unwrap_or_default();
                            if let Some(key) = active.into_iter().next() {
                                // active_tx_qsos keys are built via
                                // super::active_tx_qso_key(&qso_id.to_string())
                                // (see coordinator/qso.rs) — recover the raw
                                // qso_id string the same way that key was
                                // built, and the current offset from
                                // active_tx_offsets.
                                if let Some(qso_id) = super::active_tx_qso_id_from_key(&key) {
                                    let current = active_tx_offsets
                                        .read()
                                        .ok()
                                        .and_then(|m| m.get(&key).copied())
                                        .unwrap_or(1500.0);
                                    if let Ok(mut pending) = cmd_pending_qso_offset_requests.lock() {
                                        pending.push((
                                            qso_id,
                                            pancetta_qso::qso_manager::OffsetAction::Switch {
                                                avoid_hz: current,
                                            },
                                        ));
                                    }
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: "Nudging active QSO to a new offset".to_string(),
                                        },
                                    );
                                }
                            } else {
                                cmd_pending_cq_offset_nudge.store(true, Ordering::Relaxed);
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "TX".to_string(),
                                        status: "Nudging CQ offset (or nothing to nudge if not hunting)"
                                            .to_string(),
                                    },
                                );
                            }
                        }
```

Check whether `super::active_tx_qso_key` has an existing inverse (`active_tx_qso_id_from_key` or
similar) already in `coordinator/mod.rs` — if it stores something other than a round-trippable
`QsoId` string (e.g. it's a composite key), you'll need to either add a small inverse helper next
to `active_tx_qso_key` or change the "active QSO" branch to look up the id a different way (e.g.
if `active_tx_qsos` only ever holds exactly the callsign or a `QsoId` already, adjust the
`Option<QsoId>` parse accordingly — check the actual key format in `active_tx_qso_key`'s
definition before assuming a round-trip parse works). This step's exact plumbing depends on that
existing helper's real shape — read it first, don't guess.

Capture `pending_qso_offset_requests`/`pending_cq_offset_nudge` as `cmd_pending_qso_offset_
requests`/`cmd_pending_cq_offset_nudge` clones at the top of the relay task, same pattern as
`cmd_tx_offset_hold_hz` etc.

- [ ] **Step 12: Run tests to verify they pass**

Run: `cargo test -p pancetta nudge_forces_switch_for_active_qso nudge_sets_cq_flag`
Expected: PASS.

- [ ] **Step 13: Wire the CQ-nudge flag into the Autonomous tick loop**

In `pancetta/src/coordinator/autonomous.rs`, alongside where `pending_autonomous_cq_dispatch_
failures`/`pending_qso_offset_requests` are drained each tick (before `op.decide()` is called,
~line 1495), add:

```rust
                            if pending_cq_offset_nudge.swap(false, Ordering::Relaxed) {
                                op.request_manual_switch();
                            }
```

with `pending_cq_offset_nudge` cloned into the spawned task the same way as the other pending-*
fields (Task 9 Step 5's pattern).

- [ ] **Step 14: Run full coordinator + qso test suites, commit**

Run: `cargo test -p pancetta --features transmit`
Run: `cargo test -p pancetta-qso --features transmit`
Expected: PASS.

```bash
git add pancetta/src/coordinator/mod.rs pancetta/src/coordinator/tui_relay.rs pancetta/src/coordinator/autonomous.rs
git commit -m "$(cat <<'EOF'
feat(coordinator): wire NudgeTxOffset to the active-QSO mailbox or CQ nudge flag

PAN-72: prefers forcing a Switch for whatever QSO is in active_tx_qsos;
falls back to request_manual_switch() for the CQ-hunting case via a
one-shot AtomicBool the Autonomous task consumes before calling decide().
EOF
)"
```

---

### Task 11: Full workspace verification

**Files:** none (verification only)

- [ ] **Step 1: Full workspace test run**

Run: `cargo test --workspace --features transmit`
Expected: PASS. Investigate and fix any failure before proceeding — do not skip or `#[ignore]`
a failing test to get this task done.

- [ ] **Step 2: Format check**

Run: `cargo fmt --check`
Expected: clean. If not, run `cargo fmt` and re-run the workspace test suite from Step 1 (a
formatting pass should never change behavior, but re-verify rather than assume).

- [ ] **Step 3: Loopback integration test**

Run: `cargo test -p pancetta --test loopback_qso`
Expected: PASS unchanged — this plan doesn't touch the encode/modulate/decode path, only offset
bookkeeping and selection, so this is a regression check, not new coverage.

- [ ] **Step 4: Manual sanity note**

This feature cannot be exercised in CI or loopback tests (both need a genuinely silent or
slow-to-respond second real station). Add a line to the PR description (when opened) flagging
that an on-air sanity check of both the auto-stall-switch and the `u` keystroke is still owed,
consistent with how recent PAN tickets (PAN-65 through PAN-67) have flagged the same gap.

- [ ] **Step 5: Final commit if Step 2 produced changes**

```bash
git add -A
git commit -m "style: cargo fmt"
```

(Skip this step entirely if `cargo fmt --check` was already clean in Step 2.)
