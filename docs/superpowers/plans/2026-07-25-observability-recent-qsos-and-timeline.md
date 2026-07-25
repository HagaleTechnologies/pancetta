# Observability: Recent-QSOs panel + timeline persistence — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the two unblocked pieces of `docs/observability-diagnostics-plan.md` Layer 2: a structured, scannable Recent-QSOs outcome panel, and durable per-QSO timeline persistence. (The third remaining piece, `qso.security` Layer-1 emission, needs a real QSO-state-machine architecture change and is explicitly OUT of scope for this plan — see `project_observability_remaining_layers_scoped` memory.)

**Spec:** `docs/observability-diagnostics-plan.md` §Layer 2 — read it first. PR #84 already shipped the diagnostic-event bus foundation and the raw `QsoFailed`/`QsoCompleted` diagnostic emission this plan builds on top of.

## Global Constraints

- **Additive-only.** No change to existing decode/QSO/TX behavior — this is new bus variants, new panels, and a new persistence path only.
- **Re-verify line numbers.** The spec doc's cited line numbers (`qso.rs:1881`, `app.rs:618`, `qso_manager.rs:1965/3208/3693`, `states.rs:222`, `async_database.rs:868`) are from an earlier snapshot and may have drifted — grep to confirm exact current locations before editing.
- **Bus volume:** do not emit a diagnostic/outcome event per decode — only on QSO state changes (`QsoFailed`/`QsoCompleted`), matching PR #84's existing emission sites.
- **DB growth (timeline persistence):** gate persistence behind a new config flag (default off is acceptable if the config-merge task doesn't land in time; default value is the implementer's call, document the choice).
- **Config-merge rule:** any new config field added in this plan MUST have its `merge_with` line and a regression test in the same task (2026-07-05 bug class — see `feedback_config_merge_bug_fixed` / CLAUDE.md guardrail).
- **Subagent rules (standing):** implementers never push / never destructive git; controller pushes at batch boundaries. Local `cargo fmt` + `cargo clippy` before each commit.
- **Bounded ring:** the Recent-QSOs ring caps at 50 entries (spec's number), oldest evicted first — mirror the existing `diagnostic_events: VecDeque` (cap 500) eviction pattern in `app.rs`.

---

## Task 1: `RecentQsoOutcome` bus variant + emission at the QSO outcome handlers

**Files:**
- Modify: `pancetta/src/message_bus.rs` (new `MessageType::RecentQsoOutcome` variant, near the existing `DiagnosticEvent` variant)
- Modify: `pancetta/src/coordinator/qso.rs` (the `QsoFailed`/`QsoCompleted` handlers — grep for where PR #84 added the `DiagnosticEvent` emission for these two events and add a sibling `RecentQsoOutcome` emission right next to it)
- Test: new unit test in `qso.rs`'s test module or `coord_sim` (mirror however PR #84's diagnostic-emission tests are structured).

**Interfaces:**
- Produces:
```rust
pub struct RecentQsoOutcome {
    pub callsign: String,
    pub outcome: QsoOutcome, // enum: Failed(QsoFailureReason) | Completed
    pub last_state: String,  // or the existing state enum's Display, match what's idiomatic here
    pub freq_hz: u32,
    pub ts: DateTime<Utc>,   // match the crate's existing timestamp type
    pub brief_timeline: Vec<String>, // short human-readable summary, NOT the full state_history (that's Task 4)
}
```
Reuse existing types (`QsoFailureReason`, whatever frequency/timestamp types the coordinator already uses) rather than inventing new ones — check `qso.rs`'s existing `QsoFailed`/`QsoCompleted` handler bodies for what's already in scope at that point.

- [ ] **Step 1: Failing test** — drive a QSO to `Failed{Timeout}` in a coord_sim-style test and assert a `RecentQsoOutcome` message was sent on the bus with the correct callsign + reason. Also test the `Completed` path.
- [ ] **Step 2: Add the bus variant and construct it in both handlers**, alongside (not replacing) the existing `DiagnosticEvent` emission.
- [ ] **Step 3: Verify + benchmark not needed (no perf-sensitive path); run** `cargo test -p pancetta --features transmit 2>&1 | tail -20`.
- [ ] **Step 4: Commit** — `git commit -m "feat(coordinator): emit RecentQsoOutcome on QSO Failed/Completed"`.

---

## Task 2: TUI `recent_qsos` ring + relay wiring

**Files:**
- Modify: `pancetta-tui/src/app.rs` (new `recent_qsos: VecDeque<RecentQsoOutcome>` field, capped at 50, mirroring `diagnostic_events`'s eviction pattern near `app.rs:618`)
- Modify: `pancetta/src/coordinator/tui_relay.rs` (forward `RecentQsoOutcome` bus messages to a new `TuiMessage::RecentQsoOutcome` variant, mirroring the existing `DiagnosticEvent` forwarding arm — grep for it, it's near the line the spec doc cites as `486-520`, re-verify)
- Test: relay round-trip unit test mirroring the existing diagnostic-event relay test.

**Interfaces:**
- Consumes Task 1's `RecentQsoOutcome`.
- Produces: `App.recent_qsos: VecDeque<RecentQsoOutcome>` (cap 50), push-front-or-push-back consistent with however `diagnostic_events` orders newest (match that convention exactly so Task 3's render code and any future filter logic behave the same as the existing panel).

- [ ] **Step 1: Failing test** — send a `RecentQsoOutcome` bus message through the relay, assert `App.recent_qsos` gets the new entry and evicts the oldest once past 50.
- [ ] **Step 2: Implement** the field + relay arm.
- [ ] **Step 3: Verify** `cargo test -p pancetta-tui --features transmit 2>&1 | tail -20` and the relevant `pancetta` relay tests.
- [ ] **Step 4: Commit** — `git commit -m "feat(tui): relay RecentQsoOutcome into a bounded App ring"`.

---

## Task 3: Recent-QSOs panel render

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs` (new render fn, e.g. `render_recent_qsos_panel`, following the existing `render_diagnostics_overlay` pattern for keybinding/toggle/layout conventions)
- Modify: wherever panel keybindings are dispatched (same file(s) Task 15 of the decoder plan touches for key handling, or wherever `Shift+D`/`Shift+S` are bound — grep for those to find the pattern) — bind a new key for this panel; re-run the same free-key audit approach (`rg "Char\('X'\)"`) before picking one.
- Test: snapshot/gold test of the panel render for a mixed outcome stream (mirror however the Diagnostics panel's snapshot test — if one exists — is structured; if none exists, a plain render-doesn't-panic + content-assertion test is sufficient).

**Interfaces:**
- Consumes Task 2's `App.recent_qsos`.
- Row format per spec: `callsign, outcome + reason, final rung reached, duration` — e.g. "KJ5NJF — Failed: Timeout at SendingReport, 12 calls."

- [ ] **Step 1: Key audit** — find an unbound key, document the choice.
- [ ] **Step 2: Implement the render fn + toggle**, matching the Diagnostics panel's visual conventions (scrollable, color-coded by outcome: green=Completed, colored-by-reason for Failed).
- [ ] **Step 3: Test** — render with a synthetic mixed stream, assert the expected rows appear in the output buffer.
- [ ] **Step 4: Manual smoke** — `cargo run --release -- --headless` isn't useful for a TUI panel; note in the report that on-air/manual TUI verification is still needed (this is a UI-only gap the operator should check).
- [ ] **Step 5: Commit** — `git commit -m "feat(tui): Recent-QSOs outcome panel"`.

---

## Task 4: Persist per-QSO timeline (state_history + messages)

**Files:**
- Modify: `pancetta-qso/src/states.rs` (re-verify around line 222 — stop whatever currently discards `state_history` there)
- Modify: `pancetta-qso/src/qso_manager.rs` (re-verify around line 3693 — stop dropping `state_history`/`messages` when the QSO leaves the active map)
- Modify: `pancetta/src/coordinator/async_database.rs` (re-verify around line 868 — stop hard-coding `state_history: vec![]` on DB/ADIF writes; write the real data)
- Create or modify: a new `qso_events` table (or JSON-per-QSO sidecar file — implementer's call per the spec's "optional but high-value" framing; document the choice and why) for persisted timelines
- New config flag gating persistence (e.g. `[database].persist_qso_timeline: bool`, default your call — document it) — **must include `merge_with` + a regression test in this same task**.
- Test: write + reload a QSO's `state_history` round-trip.

**Interfaces:**
- Whatever shape lets a completed/failed QSO's full state-transition + message timeline be reconstructed offline, keyed by QSO id (or callsign+band+timestamp if there's no stable id at persistence time — check `QsoProgress`'s existing identity fields).

- [ ] **Step 1: Grep to re-locate the three discard sites** (`states.rs`, `qso_manager.rs`, `async_database.rs`) — confirm current line numbers and exact current behavior before changing anything.
- [ ] **Step 2: Add the config flag** (`merge_with` + regression test in this step, not deferred).
- [ ] **Step 3: Failing test** — write a QSO with a non-trivial `state_history`/`messages` sequence, persist it, reload, assert equality.
- [ ] **Step 4: Implement** — stop discarding at all three sites; wire the actual write path (table or sidecar, gated by the config flag).
- [ ] **Step 5: Verify** `cargo test --workspace --features transmit 2>&1 | tail -30` (this touches `pancetta-qso` and `pancetta`'s DB layer — run the full suite, not just the touched crates).
- [ ] **Step 6: Commit** — `git commit -m "feat(qso): persist per-QSO state_history/messages instead of discarding (Layer 2 timeline persistence)"`.

---

## Task 5: Docs

**Files:**
- Modify: `docs/observability-diagnostics-plan.md` (status header: mark Recent-QSOs panel + timeline persistence shipped; `qso.security` Layer-1 emission remains the only open item)
- Modify: `CLAUDE.md` if a new operator-facing keybinding or config flag warrants a line (keep under the ~100-line budget — trim something else if adding)

- [ ] **Step 1: Update the status header** in `docs/observability-diagnostics-plan.md` per the actual shipped state.
- [ ] **Step 2: Full workspace suite one final time:** `cargo test --workspace --features transmit`.
- [ ] **Step 3: Commit docs.**

---

## Self-review notes (author)

- Task 1→2→3 is a strict dependency chain (bus variant → relay → render); Task 4 is independent of 1-3 (different crate, different files) and could run concurrently with the 1-3 chain if dispatched by a separate implementer thread, per `feedback_implementer_thread_worktree_discipline` (different files, no isolation needed since there's no shared-file write conflict with 1-3).
- Deliberately deferred: `qso.security` Layer-1 emission (needs its own design pass — the QSO state machine currently has no return-signal path for a rejected message, per `project_observability_remaining_layers_scoped`).
