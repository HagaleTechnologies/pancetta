# TUI Keyboard Remap

**Date:** 2026-04-29
**Author:** K5ARH (with Claude)
**Status:** Draft — pending review

## Goal

Replace the function-key and Ctrl-chord bindings in the TUI with a
single-letter scheme so pancetta is fully usable from remote desktops,
tmux/SSH sessions, and virtual / mobile keyboards. F-keys are
unreliable across these surfaces (often hijacked by the terminal,
suppressed by hosts, or simply missing on virtual keyboards), and
Ctrl-Q is laggy or impossible on the same surfaces.

## Non-Goals

- A command palette (originally proposed as option C). Single-letter
  scheme is sufficient for the current command surface; revisit if
  the command surface grows past ~15 letters.
- Configurable keybindings. All operators agree to the same map.
- Vim-style modal command parsing (e.g., `:quit`). The single-letter
  map is the whole interface.

## Architecture

This is a key-binding-only change in
`pancetta-tui/src/tui_runner.rs::handle_key_event`. No new message
types, no coordinator changes, no widget changes. One small UI
addition: a confirm-quit modal so `q` doesn't kill an in-flight QSO
on a misclick.

## Tech Stack

ratatui (existing), crossterm (existing). No new dependencies.

## The Map

**Action keys (modal-aware: only fire when no text-input field has
focus — the existing chat-input pathway already handles this).**

| Key       | Action                                                 |
|-----------|--------------------------------------------------------|
| `Space`   | Call selected station (unchanged)                      |
| `c`       | Start repeating CQ                                     |
| `s`       | Stop CQ                                                |
| `t`       | Find clear TX offset (auto-pick — unchanged from prior) |
| `T`       | Tune — 12s tone at TX offset (Shift-required to discourage accidental TX) |
| `h`       | Halt current TX without exiting                        |
| `p`       | Toggle PTT manually                                    |
| `d`       | Open audio device picker                               |
| `q`       | Quit (with confirmation)                               |
| `?`       | Toggle help overlay                                    |

**Navigation (unchanged).**

| Key             | Action                              |
|-----------------|-------------------------------------|
| `Tab`           | Cycle active panel                  |
| `↑` / `↓`       | Move selection within active panel  |
| `Esc`           | Dismiss overlay / cancel modal      |

**Adjustments (unchanged from prior, with one tweak).**

| Key             | Action                              |
|-----------------|-------------------------------------|
| `[` / `]`       | TX offset −50 / +50 Hz              |
| `=` / `-`       | Frequency band up / down (was `+` / `-`; `=` removes Shift) |

## Removed Bindings

| Old binding | Replaced by |
|-------------|-------------|
| `F1`        | `?`         |
| `F2`        | `c`         |
| `F3`        | `s`         |
| `F4`        | `T` (Shift-T) |
| `F8`        | `h`         |
| `F9`        | `p`         |
| `Ctrl-Q`    | `q` (with confirm modal) |
| `+`         | `=`         |

`Esc` no longer quits — it only dismisses overlays / modals. (Ctrl-Q
also no longer quits; it's purely `q` now.)

## Quit Confirmation Modal

When the operator presses `q` in any non-input panel:

1. Render a modal overlay centered in the terminal, ~30 columns ×
   5 rows.
2. Title: `Quit pancetta?`
3. Body: `Quit pancetta? [y/N]` (capital N hint signals default-no
   if the operator hits Enter).
4. Key handling while modal visible:
   - `y` / `Y` / `Enter` → confirm quit (existing `TuiCommand::Quit`
     path).
   - `n` / `N` / `Esc` / `q` again → dismiss modal, return to TUI.
   - All other keys are swallowed (no panel keybindings fire while
     the modal is up).
5. The modal piggybacks on the existing modal infrastructure
   (`pancetta-tui/src/widgets/mod.rs::Modal`) so we don't reinvent
   rendering — just instantiate it with the right title/body/buttons.

The modal also serves as a sanity check for unattended operation:
operators sometimes hit keys by accident on mobile SSH clients;
adding a single-keypress confirmation prevents accidental QSO drops.

## File-by-File Changes

1. **`pancetta-tui/src/tui_runner.rs`**
   - Remove `KeyCode::F(1) | KeyCode::Char('?')` arm; replace with
     just `KeyCode::Char('?')`.
   - Remove `KeyCode::F(2)` (StartCq), `KeyCode::F(3)` (StopCq),
     `KeyCode::F(4)` (ToggleTune), `KeyCode::F(8)` (StopTx),
     `KeyCode::F(9)` (TogglePtt), `KeyCode::Char('q') if Ctrl`
     (Quit).
   - Add `KeyCode::Char('c')` → `TuiCommand::StartCq`.
   - Add `KeyCode::Char('s')` → `TuiCommand::StopCq`.
   - Add `KeyCode::Char('T')` (uppercase only) → `TuiCommand::ToggleTune`.
     Lowercase `t` already maps to `TuiCommand::FindClearOffset`
     (Task #40); leave that intact.
   - Add `KeyCode::Char('h')` → `TuiCommand::StopTx`.
   - Add `KeyCode::Char('p')` → `TuiCommand::TogglePtt`.
   - Add `KeyCode::Char('q')` → open quit-confirm modal (NOT
     `TuiCommand::Quit` directly).
   - Replace `KeyCode::Char('+')` with `KeyCode::Char('=')`.
   - When the quit-confirm modal is visible (a new `quit_confirm_visible:
     bool` field on `TuiRunner` or `App`), route keys through a
     dedicated handler that only accepts `y`/`Y`/`Enter`/`n`/`N`/`Esc`/`q`.
   - Existing `Esc` handling: keep dismissing overlays (help, device
     picker, quit-confirm). Esc no longer fires Quit.

2. **`pancetta-tui/src/app.rs`**
   - Add `pub quit_confirm_visible: bool` field on `App`, default
     `false`. Initialize alongside `help_visible`.
   - The TUI ui module reads this flag and renders the modal.

3. **`pancetta-tui/src/ui/mod.rs`**
   - In the main render fn, after rendering the existing `help_visible`
     overlay (if shown), also render a `quit_confirm_visible` overlay
     using the existing `Modal` widget.

4. **`README.md`**
   - Replace the "How to drive the TUI" key table entirely with the
     new map. Note the migration in a one-paragraph callout.

5. **`docs/RUNBOOK.md`**
   - Replace any F-key references in the Phase 5 procedure with the
     new keys. Anywhere the runbook says "F4 Tune" → "Shift-T (Tune)";
     "F8 halt" → "h"; "Ctrl-Q" → "q".

## Behavior Around Text Input

The existing TUI has a `tx_input_buffer` for typing chat / freeform
TX messages. When that buffer has focus, letter keys ARE typed into
the buffer — they don't fire commands. The current code already
distinguishes; the new bindings inherit that distinction.

The `Space` key is the most ambiguous: it currently triggers "call
selected station" when no text-input is focused, and adds a literal
space to the buffer when one is. This behavior is preserved.

## Testing

### Unit tests in `pancetta-tui/src/tui_runner.rs`'s test module

- `key_c_emits_start_cq`
- `key_s_emits_stop_cq`
- `key_uppercase_t_emits_toggle_tune`
- `key_lowercase_t_does_not_emit_toggle_tune` (it goes to
  `FindClearOffset`)
- `key_h_emits_stop_tx`
- `key_p_emits_toggle_ptt`
- `key_q_opens_quit_confirm_modal_does_not_quit`
- `key_q_in_modal_y_confirms_quit`
- `key_q_in_modal_n_dismisses`
- `key_q_in_modal_esc_dismisses`
- `key_q_in_modal_enter_confirms`
- `key_equals_emits_frequency_up`
- `key_minus_emits_frequency_down`
- `key_f4_no_longer_does_anything` (regression guard)
- `key_ctrl_q_no_longer_quits` (regression guard)
- `key_esc_does_not_quit` (regression guard)

### Manual

- Boot pancetta over SSH/tmux on the MiniPC; verify every key fires
  on a remote keyboard. Confirm F-keys do nothing.
- Press `q`, see modal, press `n` — modal dismisses, app stays
  running. Press `q` again, press `y` — app quits.
- Open chat input (Space + type into buffer); verify letter keys
  type, no commands fire. Press Enter to send.

## Out of Scope (Deferred)

- A `:` command palette as a fallback for unbound or future commands.
  Defer until the command set grows past ~15 single-letter slots.
- Configurable keymap in `~/.pancetta/config.toml`. Defer indefinitely;
  no operator has asked for it.
- Vim-style chained commands (`gg`, `dd`, etc.). Single-letter is
  enough.
- Documentation of the `[` / `]` / `=` / `-` adjustment keys in
  the RUNBOOK Phase 5 section. They're already in the README key
  table, which is the authoritative spot.
