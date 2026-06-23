# TUI Keyboard Remap Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace F-keys + Ctrl-Q with a single-letter scheme so pancetta is fully usable on remote desktops, tmux, virtual keyboards, and mobile SSH. Add a quit-confirm modal to prevent accidental exit.

**Architecture:** Key-binding-only change in `pancetta-tui/src/tui_runner.rs::handle_key_event`. New `App::quit_confirm_visible: bool` flag drives a confirm modal rendered via the existing `Modal` widget. Help overlay text + README key table updated to match.

**Tech Stack:** ratatui, crossterm. No new dependencies.

**Spec:** `docs/superpowers/specs/2026-04-29-keyboard-remap-design.md`

---

## File Map

**Modify:**
- `pancetta-tui/src/app.rs` — add `quit_confirm_visible` field, drop legacy `t` (theme) / `p` (pause) / `a` / `m` letter handlers from `App::handle_key_event` (consolidate to runner).
- `pancetta-tui/src/tui_runner.rs` — the meaty file. Remove F1-F5/F8/F9/Ctrl-Q/'+'/'D' (uppercase) arms; add c/s/h/p/P/a/m/d/q/x arms; split `t` (FindClearOffset) from `T` (ToggleTune); render quit-confirm modal in `render_frame`.
- `pancetta-tui/src/widgets/mod.rs` — already has a generic `Modal` type; reuse it. No edits expected unless a small builder method is missing.
- `README.md` — replace "How to drive the TUI" key table.
- `docs/RUNBOOK.md` — replace any F-key references in Phase 5 procedure.

**No changes:**
- `pancetta-tui/src/main.rs` — standalone dev binary. Keep in sync as a "nice to have" but out of scope for this plan.

---

### Task 1: Add `quit_confirm_visible` state + render the modal

**Files:**
- Modify: `pancetta-tui/src/app.rs` (add field around line 355)
- Modify: `pancetta-tui/src/tui_runner.rs` (add render call in `render_frame` ~ line 580)

- [ ] **Step 1: Add the field to `App` struct**

In `pancetta-tui/src/app.rs`, find the line `pub help_visible: bool,` (around line 355). Add immediately after it:

```rust
/// True while the operator-confirm-quit modal is visible. `q` opens
/// it; `y`/`Enter` confirms (sends `TuiCommand::Quit`); `n`/`Esc`/`q`
/// dismisses. Modal blocks all other keys while visible.
pub quit_confirm_visible: bool,
```

In the `App::new` (or `Default` impl) initializer block (around line 440 — search for `help_visible: false,`), add:

```rust
quit_confirm_visible: false,
```

- [ ] **Step 2: Write a failing test for the field default**

Add to `pancetta-tui/src/app.rs`'s existing `#[cfg(test)] mod tests`:

```rust
#[tokio::test]
async fn quit_confirm_visible_defaults_false() {
    let app = App::new(Config::default(), None).await.unwrap();
    assert!(!app.quit_confirm_visible);
}
```

- [ ] **Step 3: Run the test to confirm it passes**

Run: `cargo test -p pancetta-tui --lib quit_confirm_visible_defaults_false 2>&1 | tail -10`
Expected: `1 passed; 0 failed`.

- [ ] **Step 4: Add a helper `Modal` instantiation in `tui_runner.rs`**

In `pancetta-tui/src/tui_runner.rs`, add a new private method on `TuiRunner` near `render_help_overlay` (~ line 716):

```rust
fn render_quit_confirm_overlay(f: &mut Frame, area: Rect) {
    use ratatui::text::{Line, Span};

    let lines = vec![
        Line::from(""),
        Line::from(Span::styled(
            "  Quit pancetta?  [y/N]",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(""),
        Line::from(Span::styled(
            "  y / Enter = quit    n / Esc / q = cancel",
            Style::default().fg(Color::DarkGray),
        )),
    ];

    let modal_width: u16 = 50;
    let modal_height: u16 = lines.len() as u16 + 2;
    let modal_width = modal_width.min(area.width.saturating_sub(4));
    let modal_height = modal_height.min(area.height.saturating_sub(4));
    let modal_area = Rect {
        x: (area.width.saturating_sub(modal_width)) / 2,
        y: (area.height.saturating_sub(modal_height)) / 2,
        width: modal_width,
        height: modal_height,
    };

    f.render_widget(ratatui::widgets::Clear, modal_area);

    let block = Block::default()
        .title(" Confirm Quit ")
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .style(Style::default().bg(Color::Black).fg(Color::Red));

    let para = Paragraph::new(lines).block(block);
    f.render_widget(para, modal_area);
}
```

- [ ] **Step 5: Wire the render in `render_frame`**

In `pancetta-tui/src/tui_runner.rs::render_frame` (~ line 564), find the block:

```rust
// Render help overlay if visible
if app.help_visible {
    TuiRunner::render_help_overlay(f, f.area());
}
```

Add immediately after it:

```rust
// Render quit-confirm overlay if visible (drawn last so it sits on top)
if app.quit_confirm_visible {
    TuiRunner::render_quit_confirm_overlay(f, f.area());
}
```

- [ ] **Step 6: Build to verify**

Run: `cargo build -p pancetta-tui 2>&1 | tail -10`
Expected: clean build.

- [ ] **Step 7: Run all pancetta-tui tests**

Run: `cargo test -p pancetta-tui --lib 2>&1 | tail -10`
Expected: existing tests pass + new `quit_confirm_visible_defaults_false`.

- [ ] **Step 8: Commit**

```bash
git add pancetta-tui/src/app.rs pancetta-tui/src/tui_runner.rs
git commit -m "feat(tui): add quit-confirm modal infrastructure

App gains a quit_confirm_visible flag; TuiRunner renders the modal
overlay last so it sits on top of help / device-picker. The keypress
plumbing comes in the next task.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 2: Letter-key remap + remove F-keys + Ctrl-Q + dead handlers

This is the largest task. We're rewriting the `match key.code` block in `TuiRunner::handle_key_event` and removing the dead letter handlers from `App::handle_key_event`.

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs:404-558` (the `match key.code` block).
- Modify: `pancetta-tui/src/app.rs:555-575` (remove the `t`/`m`/`a`/`p` letter arms in `App::handle_key_event`).

- [ ] **Step 1: Write failing tests for the new bindings**

Add to `pancetta-tui/src/tui_runner.rs`'s existing test module (or create one if absent — search for `#[cfg(test)]` near the bottom of the file). The tests need a way to drive `handle_key_event`; the existing test pattern (if any) shows how. If no tests exist yet for `handle_key_event`, add a small fixture helper:

```rust
#[cfg(test)]
mod key_tests {
    use super::*;
    use crate::app::App;
    use crate::config::Config;
    use crossbeam_channel::unbounded;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
    use std::sync::Arc;
    use tokio::sync::RwLock;
    use std::sync::atomic::AtomicBool;

    async fn make_runner() -> (TuiRunner, crossbeam_channel::Receiver<TuiCommand>, Arc<RwLock<App>>) {
        let app = Arc::new(RwLock::new(
            App::new(Config::default(), None).await.unwrap(),
        ));
        let (tui_msg_tx, tui_msg_rx) = unbounded::<TuiMessage>();
        let (cmd_tx, cmd_rx) = unbounded::<TuiCommand>();
        let shutdown = Arc::new(AtomicBool::new(false));
        let runner = TuiRunner::new(
            Arc::clone(&app),
            Config::default(),
            tui_msg_rx,
            cmd_tx,
            shutdown,
        )
        .unwrap();
        (runner, cmd_rx, app)
    }

    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    fn key_shift(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
    }

    #[tokio::test]
    async fn key_c_emits_start_cq() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('c')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::StartCq)));
    }

    #[tokio::test]
    async fn key_s_emits_stop_cq() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('s')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::StopCq)));
    }

    #[tokio::test]
    async fn key_h_emits_stop_tx() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('h')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::StopTx)));
    }

    #[tokio::test]
    async fn key_p_emits_toggle_ptt() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('p')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::TogglePtt)));
    }

    #[tokio::test]
    async fn key_uppercase_t_emits_toggle_tune() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key_shift('T')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::ToggleTune)));
    }

    #[tokio::test]
    async fn key_lowercase_t_does_not_emit_toggle_tune() {
        // Lowercase t is FindClearOffset (handled locally; no command sent).
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('t')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn key_q_opens_modal_does_not_quit() {
        let (mut r, cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('q')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err(), "must not send Quit yet");
        assert!(app.read().await.quit_confirm_visible, "modal must be visible");
    }

    #[tokio::test]
    async fn key_y_in_modal_confirms_quit() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(key('y')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::Quit)));
    }

    #[tokio::test]
    async fn key_n_in_modal_dismisses() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(key('n')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err(), "must not Quit");
        assert!(!app.read().await.quit_confirm_visible);
    }

    #[tokio::test]
    async fn key_esc_in_modal_dismisses() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err());
        assert!(!app.read().await.quit_confirm_visible);
    }

    #[tokio::test]
    async fn key_q_in_modal_dismisses() {
        // Pressing q again while modal is up should dismiss, not stack another modal.
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(key('q')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err());
        assert!(!app.read().await.quit_confirm_visible);
    }

    #[tokio::test]
    async fn key_enter_in_modal_confirms() {
        let (mut r, cmd_rx, app) = make_runner().await;
        app.write().await.quit_confirm_visible = true;
        r.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::Quit)));
    }

    #[tokio::test]
    async fn key_d_lowercase_opens_device_picker() {
        let (mut r, _cmd_rx, app) = make_runner().await;
        r.handle_key_event(key('d')).await.unwrap();
        assert!(app.read().await.device_selection.visible);
    }

    #[tokio::test]
    async fn key_d_uppercase_no_longer_opens_device_picker() {
        // Spec says lowercase d only.
        let (mut r, _cmd_rx, app) = make_runner().await;
        r.handle_key_event(key_shift('D')).await.unwrap();
        assert!(!app.read().await.device_selection.visible);
    }

    #[tokio::test]
    async fn key_x_emits_clear_messages() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('x')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::ClearMessages)));
    }

    #[tokio::test]
    async fn key_f4_no_longer_does_anything() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(KeyEvent::new(KeyCode::F(4), KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn key_ctrl_q_no_longer_quits() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::CONTROL))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err(), "Ctrl-Q must no longer quit");
    }

    #[tokio::test]
    async fn key_esc_does_not_quit_when_no_modal() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .await
            .unwrap();
        assert!(cmd_rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn key_equals_emits_band_up() {
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('=')).await.unwrap();
        assert!(matches!(cmd_rx.try_recv(), Ok(TuiCommand::SetFrequency { .. })));
    }

    #[tokio::test]
    async fn key_plus_no_longer_changes_band() {
        // Spec drops `+` as a band-up alias to remove the Shift requirement.
        let (mut r, cmd_rx, _app) = make_runner().await;
        r.handle_key_event(key('+')).await.unwrap();
        assert!(cmd_rx.try_recv().is_err());
    }
}
```

- [ ] **Step 2: Run the tests to confirm they fail**

Run: `cargo test -p pancetta-tui --lib key_tests 2>&1 | tail -25`
Expected: most tests fail to compile (new arms not added) or assert false.

- [ ] **Step 3: Rewrite `TuiRunner::handle_key_event` match block**

Open `pancetta-tui/src/tui_runner.rs`. Find the help-modal-active early-return at line 356 and the device-modal early-return at line 367. Add a new early-return for the quit-confirm modal BEFORE both, since the quit modal is the most-recent and highest-priority:

```rust
// If quit-confirm modal is visible, route keys there
if app.quit_confirm_visible {
    match key.code {
        KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
            app.quit_confirm_visible = false;
            let _ = self.message_tx.send(TuiCommand::Quit);
            return Ok(false);
        }
        KeyCode::Char('n') | KeyCode::Char('N')
        | KeyCode::Esc
        | KeyCode::Char('q') => {
            app.quit_confirm_visible = false;
            app.status_message = "Quit cancelled".to_string();
        }
        _ => {} // swallow all other keys while modal is up
    }
    return Ok(true);
}
```

Now find the main `match key.code` block at line 404 and rewrite it. The new block (replacing everything from line 404 through the closing brace of the match, around line 558):

```rust
match key.code {
    // === Quit (with confirm modal) ===
    KeyCode::Char('q') => {
        app.quit_confirm_visible = true;
        app.status_message =
            "Quit pancetta? Press y/Enter to confirm, n/Esc/q to cancel".to_string();
    }

    // === Modal shortcuts ===
    KeyCode::Char('d') => {
        app.device_selection.visible = true;
        if app.device_selection.input_devices.is_empty()
            && app.device_selection.output_devices.is_empty()
        {
            app.status_message =
                "No audio devices reported — check coordinator connection".to_string();
        } else {
            app.status_message =
                "Select audio devices (Tab to switch, Enter to confirm, Esc to cancel)"
                    .to_string();
        }
    }
    KeyCode::Char('?') => {
        app.toggle_help();
    }

    // === Panel / list navigation ===
    KeyCode::Tab => {
        app.next_panel();
    }
    KeyCode::BackTab => {
        app.previous_panel();
    }
    KeyCode::Up => {
        app.previous_item();
    }
    KeyCode::Down => {
        app.next_item();
    }
    KeyCode::Left => {
        app.tx_frequency_offset = (app.tx_frequency_offset - 50.0).max(100.0);
        app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
    }
    KeyCode::Right => {
        app.tx_frequency_offset = (app.tx_frequency_offset + 50.0).min(3000.0);
        app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
    }

    // === CQ + QSO actions ===
    KeyCode::Char('c') => {
        self.message_tx.send(TuiCommand::StartCq)?;
    }
    KeyCode::Char('s') => {
        self.message_tx.send(TuiCommand::StopCq)?;
    }
    KeyCode::Char('h') => {
        self.message_tx.send(TuiCommand::StopTx)?;
    }
    KeyCode::Char('p') => {
        self.message_tx.send(TuiCommand::TogglePtt)?;
    }

    // === Tune / clear-offset (case-sensitive) ===
    KeyCode::Char('T') => {
        // Shift-T: 12-second single-tone tune. Shift requirement is a
        // small barrier against accidental TX during keyboard fumbling.
        self.message_tx.send(TuiCommand::ToggleTune)?;
    }
    KeyCode::Char('t') => {
        // Lowercase t: find clear TX offset and jump the cursor there.
        match app.find_clear_offset() {
            Some(hz) => {
                app.tx_frequency_offset = hz;
                app.status_message = format!("TX cursor → {:.0} Hz (clear)", hz);
            }
            None => {
                app.status_message = "No clear offset found in your parity".to_string();
            }
        }
    }

    // === Autonomous controls ===
    KeyCode::Char('a') => {
        app.toggle_autonomous();
    }
    KeyCode::Char('P') => {
        // Shift-P: pause/resume autonomous (uppercase to disambiguate from p=PTT).
        app.toggle_autonomous_pause();
    }
    KeyCode::Char('m') => {
        app.toggle_monitoring().await?;
    }

    // === Display / housekeeping ===
    KeyCode::Char('x') => {
        app.clear_messages();
        self.message_tx.send(TuiCommand::ClearMessages)?;
    }

    // === TX offset bumps ===
    KeyCode::Char('[') => {
        app.tx_frequency_offset = (app.tx_frequency_offset - 50.0).max(100.0);
        app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
    }
    KeyCode::Char(']') => {
        app.tx_frequency_offset = (app.tx_frequency_offset + 50.0).min(3000.0);
        app.status_message = format!("TX offset: {:.0} Hz", app.tx_frequency_offset);
    }

    // === Band switching ===
    KeyCode::Char('=') => {
        let freq_hz = app.band_up();
        self.message_tx.send(TuiCommand::SetFrequency {
            vfo: 0,
            frequency: freq_hz,
        })?;
    }
    KeyCode::Char('-') | KeyCode::Char('_') => {
        let freq_hz = app.band_down();
        self.message_tx.send(TuiCommand::SetFrequency {
            vfo: 0,
            frequency: freq_hz,
        })?;
    }

    // === Call station / send TX message ===
    KeyCode::Char(' ') => {
        // Existing Space handler — preserved verbatim from the prior
        // implementation. Calls selected station OR adds a space to the
        // text-input buffer when one has focus.
        // (Copy the existing Space arm body here; no behavior change.)
    }

    // === Enter — send TX message or confirm input ===
    KeyCode::Enter => {
        let text = app.get_input_text();
        if !text.is_empty() {
            self.message_tx.send(TuiCommand::SendMessage { text })?;
            app.clear_input();
        }
    }

    // === Text input fallback ===
    KeyCode::Char(c) => {
        app.input_char(c);
    }
    KeyCode::Backspace => {
        app.delete_char();
    }

    _ => {}
}
```

Note for the implementer: the Space arm body (currently lines 529-538 or so) needs to be preserved verbatim — copy the existing implementation into the placeholder above. Don't change Space's behavior.

Also: remove the old `KeyCode::Char('q') if key.modifiers.contains(KeyModifiers::CONTROL)` arm at line 406 — Ctrl-Q is gone. Remove the old `KeyCode::Char('D')` arm at line 412 — replaced by lowercase `d`. Remove `KeyCode::F(1)`, `F(2)`, `F(3)`, `F(4)`, `F(5)`, `F(8)`, `F(9)` arms. Remove the `KeyCode::Char('+')` arm — only `=` for band up.

In the help-modal early-return at line 356, change:

```rust
KeyCode::Esc | KeyCode::F(1) | KeyCode::Char('?') => {
```

to:

```rust
KeyCode::Esc | KeyCode::Char('?') => {
```

(F1 is no longer a binding.)

- [ ] **Step 4: Drop dead handlers from `App::handle_key_event`**

Open `pancetta-tui/src/app.rs`. The handler at line 497 has letter arms (`t`/`m`/`a`/`p`) that don't fire in real pancetta but are confusing dead code. Remove the entire match-arm block for `KeyCode::Char('t')`, `Char('m')`, `Char('a')`, `Char('p')` (currently around lines 555-575). Keep all other arms in `App::handle_key_event` unchanged.

If removing these breaks `App::handle_key_event`'s match exhaustiveness (it shouldn't — they're explicit arms, not catch-all), the standalone `pancetta-tui/src/main.rs` test binary will simply have fewer hotkeys. Acceptable; the standalone is not the production path.

- [ ] **Step 5: Run the tests to confirm they pass**

Run: `cargo test -p pancetta-tui --lib key_tests 2>&1 | tail -30`
Expected: all key_tests pass (≥ 18 tests).

Run full TUI suite to confirm no regression:
`cargo test -p pancetta-tui --lib 2>&1 | tail -10`

- [ ] **Step 6: Build to verify no warnings**

Run: `cargo build -p pancetta-tui 2>&1 | tail -10`
Expected: clean.

Run: `cargo clippy -p pancetta-tui --lib 2>&1 | tail -25`
Expected: no new warnings.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
git add pancetta-tui/src/tui_runner.rs pancetta-tui/src/app.rs
git commit -m "feat(tui): single-letter key map; remove F-keys + Ctrl-Q

c=CQ start, s=CQ stop, h=halt TX, p=PTT, T=tune (Shift),
q=quit (with confirm modal), x=clear messages, d=device picker
(was uppercase D), a=autonomous toggle, P=autonomous pause
(Shift), m=monitoring. F1-F5/F8/F9 + Ctrl-Q + uppercase D + '+'
all removed. Esc no longer quits — only dismisses modals.

Drops the dead t/m/a/p letter arms from App::handle_key_event;
those only fired in the standalone dev binary.

Designed for remote desktop / tmux / virtual / mobile keyboards
where F-keys and Ctrl-chords are unreliable.

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 3: Update help overlay text

**Files:**
- Modify: `pancetta-tui/src/tui_runner.rs:718-736` (the `lines` array in `render_help_overlay`)
- Modify: `pancetta-tui/src/app.rs:984` (the help status message)

- [ ] **Step 1: Replace the help-overlay key list**

In `pancetta-tui/src/tui_runner.rs::render_help_overlay`, find the `lines: &[(&str, &str)]` array (around line 718). Replace its contents with:

```rust
let lines: &[(&str, &str)] = &[
    ("?", "Toggle this help"),
    ("Tab / Shift+Tab", "Switch panel"),
    ("Up / Down", "Scroll list"),
    ("Left / Right", "TX offset −/+ 50 Hz"),
    ("[ / ]", "TX offset −/+ 50 Hz"),
    ("= / -", "Band up / down"),
    ("Space", "Call selected station"),
    ("Enter", "Send TX message"),
    ("c / s", "Start / stop CQ"),
    ("t", "Find clear TX offset"),
    ("Shift+T", "Tune (12 s tone)"),
    ("h", "Halt current TX"),
    ("p", "Toggle PTT"),
    ("a", "Toggle autonomous mode"),
    ("Shift+P", "Pause / resume autonomous"),
    ("m", "Toggle audio monitoring"),
    ("d", "Device picker"),
    ("x", "Clear decoded messages"),
    ("q", "Quit (with confirm)"),
    ("Esc", "Dismiss overlay / cancel modal"),
];
```

- [ ] **Step 2: Update the help-overlay close hint**

In the same function (around line 783), change:

```rust
"  Press Escape, F1, or ? to close",
```

to:

```rust
"  Press Escape or ? to close",
```

- [ ] **Step 3: Update the status message in `App::toggle_help`**

In `pancetta-tui/src/app.rs:984`, change:

```rust
self.status_message = "Help — press Escape or F1 to close".to_string();
```

to:

```rust
self.status_message = "Help — press Escape or ? to close".to_string();
```

- [ ] **Step 4: Build + run tests**

Run: `cargo build -p pancetta-tui 2>&1 | tail -5 && cargo test -p pancetta-tui --lib 2>&1 | tail -10`
Expected: clean build, all tests pass (no new tests; this is text-only).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add pancetta-tui/src/tui_runner.rs pancetta-tui/src/app.rs
git commit -m "docs(tui): help overlay shows new single-letter keys

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 4: Update README + RUNBOOK

**Files:**
- Modify: `README.md`
- Modify: `docs/RUNBOOK.md`

- [ ] **Step 1: Replace the README key table**

In `README.md`, find the "How to drive the TUI" section. Replace the entire table with:

```markdown
| Key | Action |
|---|---|
| `Tab` / `Shift+Tab` | Cycle active panel |
| `↑` / `↓` | Move selection within active panel |
| `←` / `→` or `[` / `]` | TX offset −/+ 50 Hz |
| `=` / `-` | Band up / down |
| `Space` | Call selected station |
| `Enter` | Send the TX text in the input buffer |
| `c` / `s` | Start / stop repeating CQ |
| `t` | **Find clear TX offset** — auto-picks a 25 Hz candidate clear in your TX parity. |
| `Shift+T` | **Tune** — 12 s single tone at TX offset (PTT engages). |
| `h` | **Halt current TX** (drops PTT within ~150 ms) |
| `p` | Toggle PTT manually |
| `a` | Toggle autonomous mode |
| `Shift+P` | Pause / resume autonomous |
| `m` | Toggle audio monitoring |
| `d` | Open audio device picker |
| `x` | Clear decoded messages |
| `?` | Toggle help overlay |
| `q` | Quit (with `[y/N]` confirm) |
| `Esc` | Dismiss any overlay / cancel modal |
```

If the README has a callout for the previous keymap (with F-keys / Ctrl-Q), remove it.

- [ ] **Step 2: Update RUNBOOK Phase 5 procedure**

In `docs/RUNBOOK.md`, find any reference to F-keys or Ctrl-Q in the Phase 5 (autonomous QSO loop) procedure. Replace:

| Old | New |
|---|---|
| `F4` / `F4 Tune` | `Shift+T` (Tune) |
| `F8` | `h` (halt) |
| `Ctrl-Q` | `q` (with confirm) |
| `F2` / `F3` (start / stop CQ) | `c` / `s` |
| `F9` (PTT) | `p` |

Use Find-and-Replace with care: don't touch any references to function keys for the radio (CAT / Hamlib has its own F-key terminology that doesn't relate to TUI).

If the RUNBOOK has a "Pre-flight checks" or "Operator preparation" section, ensure the new key references are in context.

- [ ] **Step 3: Verify no remaining stale references**

Run: `grep -n "Ctrl-Q\|Ctrl+Q\|F1 /\|F2 /\|F3 /\|F4 /\|F5 /\|F8 /\|F9 /\|F1, F2\|F2, F3" README.md docs/RUNBOOK.md`
Expected: no matches.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/RUNBOOK.md
git commit -m "docs: README + RUNBOOK reflect new single-letter keymap

Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>"
```

---

### Task 5: Final integration

**Files:** none (verification only)

- [ ] **Step 1: Run the full test suite for affected crates**

Per CLAUDE.md, use plain `cargo test -p <crate>` (no `--workspace`).

Run:
```bash
cargo test -p pancetta-tui --lib 2>&1 | tail -10
cargo test -p pancetta --lib 2>&1 | tail -10
```

Expected: both clean.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy --workspace --features transmit 2>&1 | tail -30`
Expected: no errors, no new warnings on touched files.

- [ ] **Step 3: Run cargo fmt**

Run: `cargo fmt --all -- --check`
Expected: no diff.

- [ ] **Step 4: Push to origin**

```bash
git push
```

Expected: pre-push hook (scripts/check.sh) runs fmt + clippy + cargo deny; succeeds.

---

## Notes for the Implementer

- **The Space-key arm is sensitive.** The current code at `tui_runner.rs:529-538` (or thereabouts) has Space behavior tied to the active panel — it triggers "Call selected station" when in band activity, and adds a literal space to the input buffer when a chat field has focus. Preserve verbatim. Don't refactor.
- **The standalone `pancetta-tui/src/main.rs` binary** is out of scope for this plan. It uses `App::handle_key_event` directly and has its own keymap that may now diverge from the production runner. Acceptable; the standalone is a dev convenience, not a shipping target.
- **Don't break existing modals.** The help and device-picker overlays have their own early-return handlers at the top of `handle_key_event`. The new quit-confirm modal early-return must be added BEFORE both, since it should win priority over them.
- **Memory-noted policies followed by this plan:**
  - Each task ends with a commit.
  - Doc updates in Task 4.
  - Tests run autonomously per task.
  - Plain `cargo test -p <crate>` (no `--workspace`).
  - Pre-push hook honored — Task 5 runs fmt + clippy before push.
