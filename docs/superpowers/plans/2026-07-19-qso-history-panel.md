# Last-10-QSOs History Panel (#165) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add a compact line to the QSO Status panel showing the outcome of the last 10 QSOs
(success/failure), so the operator can see recent history at a glance without checking the log.

**Architecture:** A new `MessageType::QsoHistoryEntry` bus variant is pushed once per QSO reaching
a terminal state (`QsoEvent::QsoCompleted`/`QsoFailed`), from the exact same handler blocks in
`pancetta/src/coordinator/qso.rs` that already push `MessageType::ActiveQsosSnapshot` on those
same events. `tui_relay.rs` relays it to a new `TuiMessage::QsoHistoryEntry`, which `App` stores
in a capped `VecDeque<QsoHistoryItem>`. `qso_status.rs` renders the last 10 as one line in the
single-QSO-detail layout.

**Tech Stack:** Rust workspace (pancetta-qso, pancetta, pancetta-tui crates), the existing
coordinator↔TUI message-bus/`TuiMessage` relay pattern already used for `ActiveQsosSnapshot`.

## Global Constraints

- `cargo fmt` must be run for real (not just `--check`) before every commit.
- This is additive to both `MessageType` and `TuiMessage` — no existing variant or handler
  changes.
- The multi-QSO table view (`render_multi_qso_table` in `qso_status.rs`) does NOT get this line —
  only the single-QSO-detail layout, consistent with that view already being denser.
- Not color-only signaling: the ✓/✗ glyph itself carries the meaning (accessibility).

---

## File Structure

| File | Responsibility |
|---|---|
| `pancetta/src/message_bus.rs` | New `MessageType::QsoHistoryEntry` variant |
| `pancetta/src/coordinator/qso.rs` | Emit the new message from the existing `QsoCompleted`/`QsoFailed` handler blocks |
| `pancetta-tui/src/tui_runner.rs` | New `TuiMessage::QsoHistoryEntry` variant + relay match arm + dispatch to `App` |
| `pancetta/src/coordinator/tui_relay.rs` | Match arm forwarding `MessageType::QsoHistoryEntry` → `TuiMessage::QsoHistoryEntry` |
| `pancetta-tui/src/app.rs` | `QsoHistoryItem` struct, `App.qso_history: VecDeque<QsoHistoryItem>`, `push_qso_history` method |
| `pancetta-tui/src/ui/qso_status.rs` | `format_qso_history_line` pure function + render wiring |
| `docs/DECISIONS/tui.md` (or nearest equivalent) | Note the new panel line |

---

## Task 1: `MessageType::QsoHistoryEntry` bus variant

**Files:**
- Modify: `pancetta/src/message_bus.rs` (add near `ActiveQsosSnapshot`, ~line 364-371)

**Interfaces:**
- Produces: `MessageType::QsoHistoryEntry { call_sign: String, band: String, success: bool, reason: Option<String>, completed_at: chrono::DateTime<chrono::Utc> }`

- [ ] **Step 1: Add the variant**

In `pancetta/src/message_bus.rs`, find the `ActiveQsosSnapshot` variant inside `pub enum MessageType`:

```rust
    ActiveQsosSnapshot {
        qsos: Vec<ActiveQsoSnapshotItem>,
        pending: Vec<PendingCallSnapshotItem>,
    },
```

Add directly after it:

```rust
    /// Pushed once per QSO reaching a terminal state (Completed or Failed) —
    /// #165's last-10-QSOs history panel. Additive; existing
    /// `ActiveQsosSnapshot` handling is untouched.
    QsoHistoryEntry {
        call_sign: String,
        band: String,
        success: bool,
        /// Populated only when `success` is false (e.g. "Timeout").
        reason: Option<String>,
        completed_at: chrono::DateTime<chrono::Utc>,
    },
```

- [ ] **Step 2: Build to verify it compiles**

Run: `cargo build -p pancetta`
Expected: clean build (a new enum variant with no match arms referencing it yet compiles fine —
Rust only requires exhaustive matches where the enum is actually matched, and we haven't added
any `match` on `MessageType` that would need updating until Task 2/`tui_relay.rs` — check
`cargo build -p pancetta` doesn't surface a non-exhaustive-match error from some other pre-existing
match site; if it does, that match needs a wildcard arm or an explicit new arm, add
`_ => {}`-shaped handling there matching that call site's existing style before proceeding).

- [ ] **Step 3: Commit**

```bash
git add pancetta/src/message_bus.rs
git commit -m "feat(bus): add MessageType::QsoHistoryEntry for #165"
```

---

## Task 2: Emit `QsoHistoryEntry` from the coordinator's terminal-state handlers

**Files:**
- Modify: `pancetta/src/coordinator/qso.rs` (two sites: `QsoCompleted` ~line 1984, `QsoFailed` ~line 2130)

**Interfaces:**
- Consumes: `MessageType::QsoHistoryEntry` (Task 1), the existing `failure_reason_text(&reason)` helper (already used in this file for the `DiagnosticEvent` push), `pancetta_qso::utils::frequency_to_band`

- [ ] **Step 1: Add the emit in the `QsoCompleted` handler**

In `pancetta/src/coordinator/qso.rs`, find (inside the `QsoEvent::QsoCompleted` arm):

```rust
                                if let Some(ref their_call) = metadata.their_callsign {
                                    info!("QSO completed with {}, marking as worked", their_call);
```

Add directly after that `info!` line (still inside the `if let Some(ref their_call) = ...` block,
so `their_call` is in scope):

```rust
                                    let history_band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    let history_msg = ComponentMessage::new(
                                        ComponentId::Qso,
                                        ComponentId::Tui,
                                        MessageType::QsoHistoryEntry {
                                            call_sign: their_call.clone(),
                                            band: history_band,
                                            success: true,
                                            reason: None,
                                            completed_at: chrono::Utc::now(),
                                        },
                                        Instant::now(),
                                    );
                                    let _ = snapshot_bus.send_message(history_msg).await;
```

- [ ] **Step 2: Add the emit in the `QsoFailed` handler**

In the same file, find (inside the `QsoEvent::QsoFailed` arm):

```rust
                                let reason_text = failure_reason_text(&reason);
                                let diag_msg = ComponentMessage::new(
                                    ComponentId::Qso,
                                    ComponentId::Tui,
                                    MessageType::DiagnosticEvent {
                                        target: "qso",
```

Add directly BEFORE that `let reason_text = ...` line (so it doesn't shadow/reorder the existing
diagnostic push):

```rust
                                if let Some(ref their_call) = metadata.their_callsign {
                                    let history_band =
                                        pancetta_qso::utils::frequency_to_band(metadata.frequency);
                                    let history_msg = ComponentMessage::new(
                                        ComponentId::Qso,
                                        ComponentId::Tui,
                                        MessageType::QsoHistoryEntry {
                                            call_sign: their_call.clone(),
                                            band: history_band,
                                            success: false,
                                            reason: Some(failure_reason_text(&reason)),
                                            completed_at: chrono::Utc::now(),
                                        },
                                        Instant::now(),
                                    );
                                    let _ = snapshot_bus.send_message(history_msg).await;
                                }
```

(`failure_reason_text` returns `String` already — confirmed from its definition at
`pancetta/src/coordinator/qso.rs:1041`, `fn failure_reason_text(reason: &pancetta_qso::QsoFailureReason) -> String`.)

- [ ] **Step 3: Build to verify no compile errors**

Run: `cargo build -p pancetta`
Expected: clean build.

- [ ] **Step 4: Run the coordinator's qso test suite**

Run: `cargo test -p pancetta --lib coordinator::qso`
Expected: all pass — this is a pure addition alongside existing pushes, no existing assertion
should be affected.

- [ ] **Step 5: Commit**

```bash
git add pancetta/src/coordinator/qso.rs
git commit -m "feat(coordinator): emit QsoHistoryEntry on QSO completion and failure for #165"
```

---

## Task 3: `TuiMessage::QsoHistoryEntry` + relay wiring

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs` (enum variant ~near `ActiveQsosUpdate`, dispatch arm ~near line 732-737)
- Modify: `pancetta/src/coordinator/tui_relay.rs` (relay match arm, ~near line 424-448)

**Interfaces:**
- Consumes: `MessageType::QsoHistoryEntry` (Task 1)
- Produces: `TuiMessage::QsoHistoryEntry { call_sign, band, success, reason, completed_at }`, calls a new `App::push_qso_history` (implemented in Task 4 — this task's dispatch arm references it by name; if Task 4 hasn't landed yet in a from-scratch build, this crate won't compile until Task 4's method exists — that's fine within one PR's task sequence, just don't expect `cargo build -p pancetta-tui` to pass until after Task 4)

- [ ] **Step 1: Add the `TuiMessage` variant**

In `pancetta-tui/src/tui_runner.rs`, find the `TuiMessage` enum's `ActiveQsosUpdate` variant and
add directly after it (or anywhere in the enum — exact position doesn't matter, grouping near
`ActiveQsosUpdate` is just for readability):

```rust
    /// Pushed once per QSO reaching a terminal state — #165's last-10-QSOs
    /// history panel.
    QsoHistoryEntry {
        call_sign: String,
        band: String,
        success: bool,
        reason: Option<String>,
        completed_at: chrono::DateTime<chrono::Utc>,
    },
```

- [ ] **Step 2: Add the dispatch arm**

In the same file's `handle_message` function, find:

```rust
            TuiMessage::ActiveQsosUpdate {
                qsos,
                pending_calls,
            } => {
                app.apply_active_qsos(qsos, pending_calls);
            }
```

Add directly after it:

```rust
            TuiMessage::QsoHistoryEntry {
                call_sign,
                success,
                completed_at,
                ..
            } => {
                app.push_qso_history(call_sign, success, completed_at);
            }
```

- [ ] **Step 3: Add the relay match arm in `tui_relay.rs`**

In `pancetta/src/coordinator/tui_relay.rs`, find the `MessageType::ActiveQsosSnapshot { .. } => { ... }` arm and add directly after its closing brace:

```rust
                        MessageType::QsoHistoryEntry {
                            call_sign,
                            band,
                            success,
                            reason,
                            completed_at,
                        } => {
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::QsoHistoryEntry {
                                    call_sign,
                                    band,
                                    success,
                                    reason,
                                    completed_at,
                                },
                            );
                        }
```

- [ ] **Step 4: Build (expect a failure referencing Task 4)**

Run: `cargo build -p pancetta-tui`
Expected: FAIL with `no method named push_qso_history found for struct App` — this is expected at
this point in the sequence; proceed to Task 4, which implements it.

- [ ] **Step 5: Commit**

```bash
git add pancetta-tui/src/tui_runner.rs pancetta/src/coordinator/tui_relay.rs
git commit -m "feat(tui): add TuiMessage::QsoHistoryEntry and relay wiring for #165"
```

---

## Task 4: `App.qso_history` state

**Files:**
- Modify: `pancetta-tui/src/app.rs`
- Test: same file's `#[cfg(test)] mod tests` block

**Interfaces:**
- Produces: `pub struct QsoHistoryItem { pub call_sign: String, pub success: bool, pub completed_at: chrono::DateTime<chrono::Utc> }`, `App.qso_history: VecDeque<QsoHistoryItem>` (public field, read by `qso_status.rs`), `App::push_qso_history(&mut self, call_sign: String, success: bool, completed_at: chrono::DateTime<chrono::Utc>)`

- [ ] **Step 1: Write the failing test**

Add to `pancetta-tui/src/app.rs`'s test module:

```rust
#[tokio::test]
async fn push_qso_history_prepends_most_recent_first() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    let t1 = chrono::Utc::now();
    let t2 = t1 + chrono::Duration::seconds(15);
    app.push_qso_history("K5ARH".to_string(), true, t1);
    app.push_qso_history("JA1ABC".to_string(), false, t2);
    assert_eq!(app.qso_history.len(), 2);
    assert_eq!(app.qso_history[0].call_sign, "JA1ABC");
    assert!(!app.qso_history[0].success);
    assert_eq!(app.qso_history[1].call_sign, "K5ARH");
    assert!(app.qso_history[1].success);
}

#[tokio::test]
async fn push_qso_history_caps_at_ten_evicting_oldest() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    let base = chrono::Utc::now();
    for i in 0i64..11i64 {
        app.push_qso_history(
            format!("CALL{i}"),
            true,
            base + chrono::Duration::seconds(i),
        );
    }
    assert_eq!(app.qso_history.len(), 10);
    // The oldest (CALL0) was evicted; the most recent (CALL10) is at the front.
    assert_eq!(app.qso_history[0].call_sign, "CALL10");
    assert!(!app.qso_history.iter().any(|item| item.call_sign == "CALL0"));
}
```

(`App::new` is async and returns `Result`, matching this file's existing test-construction
pattern — e.g. the tests around line 3927 of `app.rs`.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-tui push_qso_history -- --nocapture`
Expected: FAIL — `QsoHistoryItem`/`push_qso_history`/`qso_history` don't exist yet.

- [ ] **Step 3: Write minimal implementation**

Add near `DecodedMessageView` (top of `pancetta-tui/src/app.rs`):

```rust
/// One entry in the last-10-QSOs history panel (#165).
#[derive(Debug, Clone)]
pub struct QsoHistoryItem {
    pub call_sign: String,
    pub success: bool,
    pub completed_at: chrono::DateTime<chrono::Utc>,
}
```

Add a field to `pub struct App` (near `decoded_messages`):

```rust
    /// Last 10 QSO outcomes, most recent first (#165). Capped in
    /// `push_qso_history`.
    pub qso_history: VecDeque<QsoHistoryItem>,
```

Initialize it in `App::new(...)` (wherever `decoded_messages: VecDeque::with_capacity(1000)` is
set):

```rust
            qso_history: VecDeque::with_capacity(10),
```

Add the method (anywhere in `impl App`, e.g. near `apply_active_qsos`):

```rust
    /// Record a QSO's terminal outcome for the last-10 history panel (#165).
    /// Most-recent-first; capped at 10, evicting the oldest.
    pub fn push_qso_history(
        &mut self,
        call_sign: String,
        success: bool,
        completed_at: chrono::DateTime<chrono::Utc>,
    ) {
        self.qso_history.push_front(QsoHistoryItem {
            call_sign,
            success,
            completed_at,
        });
        self.qso_history.truncate(10);
    }
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-tui push_qso_history -- --nocapture`
Expected: 2 passed

- [ ] **Step 5: Build the full `pancetta-tui`/`pancetta` crates to confirm Task 3's dispatch arm now compiles**

Run: `cargo build -p pancetta-tui -p pancetta`
Expected: clean build.

- [ ] **Step 6: Commit**

```bash
git add pancetta-tui/src/app.rs
git commit -m "feat(tui): add App.qso_history state for #165"
```

---

## Task 5: Render the history line in `qso_status.rs`

**Files:**
- Modify: `pancetta-tui/src/ui/qso_status.rs`
- Test: same file's `#[cfg(test)] mod tests` block

**Interfaces:**
- Consumes: `QsoHistoryItem` (Task 4)
- Produces: `pub(crate) fn format_qso_history_line(items: &[QsoHistoryItem]) -> Vec<Span<'static>>`

- [ ] **Step 1: Write the failing test**

Add to `pancetta-tui/src/ui/qso_status.rs`'s test module:

```rust
#[test]
fn format_qso_history_line_empty_is_empty() {
    assert!(format_qso_history_line(&[]).is_empty());
}

#[test]
fn format_qso_history_line_renders_glyph_per_outcome() {
    use crate::app::QsoHistoryItem;
    let items = vec![
        QsoHistoryItem {
            call_sign: "K5ARH".to_string(),
            success: true,
            completed_at: chrono::Utc::now(),
        },
        QsoHistoryItem {
            call_sign: "JA1ABC".to_string(),
            success: false,
            completed_at: chrono::Utc::now(),
        },
    ];
    let spans = format_qso_history_line(&items);
    let rendered: String = spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(rendered.contains('\u{2713}'), "expected a ✓ glyph: {rendered}"); // ✓
    assert!(rendered.contains('\u{2717}'), "expected a ✗ glyph: {rendered}"); // ✗
    assert!(rendered.contains("K5ARH"));
    assert!(rendered.contains("JA1ABC"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-tui format_qso_history_line -- --nocapture`
Expected: FAIL — function doesn't exist.

- [ ] **Step 3: Write minimal implementation**

Add to `pancetta-tui/src/ui/qso_status.rs` (near `format_queued_line`):

```rust
/// Build the last-10-QSOs history line (#165): "✓K5ARH ✗JA1ABC ✓DL5XYZ ...",
/// most recent first (the slice is already ordered that way by
/// `App::push_qso_history`). Not color-only — the ✓/✗ glyph itself carries
/// the meaning. Pure so it's directly unit-testable, following the same
/// pattern as `format_queued_line`.
pub(crate) fn format_qso_history_line(
    items: &[crate::app::QsoHistoryItem],
) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            spans.push(Span::raw(" "));
        }
        let (glyph, color) = if item.success {
            ("\u{2713}", Color::Green)
        } else {
            ("\u{2717}", Color::Red)
        };
        spans.push(Span::styled(
            format!("{glyph}{}", item.call_sign),
            Style::default().fg(color),
        ));
    }
    spans
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p pancetta-tui format_qso_history_line -- --nocapture`
Expected: 2 passed

- [ ] **Step 5: Wire it into the single-QSO-detail layout**

In `render_qso_status`, find the single-QSO-detail layout's `Layout::default()...constraints([...])`
block:

```rust
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),             // QSO info
                Constraint::Length(3),             // Sequence ladder + Now/Next
                Constraint::Length(2),             // TX/RX status
                Constraint::Length(2),             // SNR meters
                Constraint::Min(1),                // Progress/timing
                Constraint::Length(queued_height), // Queued calls (0 when empty)
                Constraint::Length(1),             // Control hint
            ])
            .split(block.inner(area));

        f.render_widget(block, area);

        render_qso_info(f, chunks[0], app);
        render_ladder(f, chunks[1], app);
        render_tx_rx_status(f, chunks[2], app);
        render_snr_meters(f, chunks[3], app);
        render_timing_progress(f, chunks[4], app);
        if !app.pending_calls.is_empty() {
            render_queued_calls(f, chunks[5], app);
        }
        render_control_hint(f, chunks[6], app);
```

Replace with (adds one `Constraint::Length(1)` row and its render call, shifting the queued/hint
indices by one):

```rust
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),             // QSO info
                Constraint::Length(3),             // Sequence ladder + Now/Next
                Constraint::Length(2),             // TX/RX status
                Constraint::Length(2),             // SNR meters
                Constraint::Min(1),                // Progress/timing
                Constraint::Length(1),             // Last-10-QSOs history (#165)
                Constraint::Length(queued_height), // Queued calls (0 when empty)
                Constraint::Length(1),             // Control hint
            ])
            .split(block.inner(area));

        f.render_widget(block, area);

        render_qso_info(f, chunks[0], app);
        render_ladder(f, chunks[1], app);
        render_tx_rx_status(f, chunks[2], app);
        render_snr_meters(f, chunks[3], app);
        render_timing_progress(f, chunks[4], app);
        render_qso_history(f, chunks[5], app);
        if !app.pending_calls.is_empty() {
            render_queued_calls(f, chunks[6], app);
        }
        render_control_hint(f, chunks[7], app);
```

Add the new render function (near `render_queued_calls`):

```rust
/// Render the last-10-QSOs history line (#165).
fn render_qso_history(f: &mut Frame<'_>, area: Rect, app: &App) {
    let items: Vec<_> = app.qso_history.iter().cloned().collect();
    let spans = format_qso_history_line(&items);
    let line = if spans.is_empty() {
        Line::from(Span::styled(
            "No QSOs yet",
            Style::default().fg(app.theme.muted_color()),
        ))
    } else {
        Line::from(spans)
    };
    f.render_widget(Paragraph::new(line), area);
}
```

- [ ] **Step 6: Run the full `pancetta-tui` test suite**

Run: `cargo test -p pancetta-tui`
Expected: all pass.

- [ ] **Step 7: Commit**

```bash
git add pancetta-tui/src/ui/qso_status.rs
git commit -m "feat(tui): render the last-10-QSOs history line in QSO Status panel (#165)"
```

---

## Task 6: Full workspace verification, docs, PR

**Files:**
- Modify: `docs/DECISIONS/tui.md` (or the nearest existing TUI-panel decisions doc — check
  `docs/DECISIONS/` for the right file before writing; if none exists specifically for TUI panels,
  append to `docs/superpowers/specs/2026-07-19-dx-hunter-priority-tiers-and-history-panel-design.md`'s
  own directory convention isn't right for a DECISIONS entry — create
  `docs/DECISIONS/tui.md` if no closer match exists)

- [ ] **Step 1: Run the full workspace test suite**

Run: `cargo test --workspace --features transmit`
Expected: all pass.

- [ ] **Step 2: Format for real and verify clean**

Run: `cargo fmt`
Run: `cargo fmt --check`
Expected: second command produces no diff.

- [ ] **Step 3: Clippy**

Run: `cargo clippy --workspace --features transmit -- -D warnings`
Expected: clean.

- [ ] **Step 4: Docs**

Add a short section to the appropriate `docs/DECISIONS/` file (per the note above):

```markdown
## Last-10-QSOs history panel (issue #165), landed 2026-07-19

The QSO Status panel's single-QSO-detail layout gained a compact history line
("✓K5ARH ✗JA1ABC ..."), most recent first, capped at 10. Wired via a new
`MessageType::QsoHistoryEntry` bus push (emitted from the coordinator's existing
`QsoEvent::QsoCompleted`/`QsoFailed` handlers, alongside the existing `ActiveQsosSnapshot` push)
relayed to a new `TuiMessage::QsoHistoryEntry`. "Successful" = `QsoCompleted`; any `QsoFailed`
reason renders the ✗ glyph (not color-only, for accessibility). The multi-QSO table view does not
get this line — only the single-QSO-detail layout.
```

- [ ] **Step 5: Commit docs**

```bash
git add docs/DECISIONS/tui.md
git commit -m "docs: record the #165 last-10-QSOs history panel"
```

- [ ] **Step 6: Push and open the PR**

```bash
git push -u origin <branch-name>
gh pr create --title "feat(tui): last-10-QSOs history panel (#165)" --body "$(cat <<'EOF'
## Summary
- Adds a compact "✓K5ARH ✗JA1ABC ..." history line to the QSO Status panel's single-QSO-detail
  layout, most recent 10 QSOs first.
- New MessageType::QsoHistoryEntry bus push, emitted from the same QsoCompleted/QsoFailed
  handlers that already push ActiveQsosSnapshot; relayed to a new TuiMessage variant.

## Test plan
- [ ] `cargo test --workspace --features transmit` green
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --workspace --features transmit -- -D warnings` clean
- [ ] On-air: complete a QSO and confirm it appears in the history line with a ✓; let one time
  out and confirm it appears with a ✗

Closes #165

🤖 Generated with [Claude Code](https://claude.com/claude-code)

https://claude.ai/code/session_01VqMW4dvjiSpBJCoprJZsug
EOF
)"
```

- [ ] **Step 7: Verify**

Run: `gh pr view --json number,url,mergeable`
Expected: PR created.
