# TUI Redesign Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Activity views (Operate/Hunt/Run/Monitor), a vacancy-first TX-placement instrument replacing the waterfall outside Monitor, and callsign-pinned global station focus with multi-TX two-tier highlighting — per the approved spec `docs/superpowers/specs/2026-07-03-tui-redesign-design.md`.

**Architecture:** All UI work lives in `pancetta-tui`; the only cross-crate additions are (a) two pure per-slot fields on `FrequencyCandidate` in `pancetta-qso`, (b) moving two pure callsign helpers from `pancetta-qso` into `pancetta-core`, and (c) an additive `TxPlacementUpdate` bus message computed in the coordinator's autonomous tick (which already owns the allocator, spectral snapshot, and decode history) and relayed to the TUI — the "single-scorer invariant": the TUI never re-derives placement scores.

**Tech Stack:** Rust, ratatui 0.30 + crossterm (existing), serde (state file), existing `MessageBus`/`TuiMessage` relay patterns.

## Global Constraints

- **Operate view must render byte-identically to today's layout until Phase-3 tasks swap in the instrument** — the existing 164 `pancetta-tui` lib tests are the regression oracle; they must stay green after every task.
- **No TX-behavior changes anywhere.** Parking flows exclusively through the existing `TuiCommand::SetTxOffset` → `tx_offset_hold_hz`/`TxFreqMode` machinery. Auto-repark (Task 16) may only rewrite the *idle parked offset*, never a live stream's frequency, and never while any QSO is active.
- All bus/TUI message additions are **additive** (new variants only; existing variants untouched).
- Every commit: `cargo fmt --all` + `cargo clippy --all-targets -p <touched crates>` clean + `cargo test -p pancetta-tui --lib` green (plus touched-crate tests). Full workspace test before each phase's final commit.
- Implementers do NOT push; the controller pushes (repo standing rule).
- Each **Phase (1–5) is one PR**; finish a phase before starting the next.
- Callsign comparisons for focus/engagement use compound-aware `callsigns_match` (moved to `pancetta-core` in Task 2), never raw `==`.
- FT8 lists mutate every ~15 s; **no positional cursor may survive a list mutation unpinned** (the invariant Tasks 1–2 establish).

## Phase status (check off as completed)

- [ ] Phase 1 (PR 1): Selection foundation — Tasks 1–3
- [ ] Phase 2 (PR 2): View scaffold + zoom — Tasks 4–7
- [ ] Phase 3 (PR 3): TX-placement instrument — Tasks 8–16
- [ ] Phase 4 (PR 4): Station card + Station Info demotion — Tasks 17–18
- [ ] Phase 5 (PR 5): Mouse + quick wins — Tasks 19–20

**Grounding notes for all tasks (verified 2026-07-03, main @ 07dbe108):**
- DX Hunter and Callers cursors are **already callsign-pinned** (`dx_hunter_pinned_call`, `callers_pinned_call`, app.rs:673-691). Only Band Activity is bare. Task 1 copies their pattern; do NOT re-implement theirs.
- The parity enum in `pancetta-qso/src/frequency.rs` is `TimeSlot::{First, Second}` (First = even 15 s slot, Second = odd). NOT `Even/Odd`.
- The coordinator's autonomous tick (coordinator/autonomous.rs:451-493) already refreshes `SpectralSnapshot` (from waterfall rows) and `DecodeHistory` (`op.feed_decoded_messages`) every slot **regardless of whether autonomous mode is enabled** — only *actions* are gated. Task 9 piggybacks there.
- `rank_candidates(spectral, &history, &own_freqs, dx_target_hz) -> Vec<FrequencyCandidate>` (frequency.rs:190) is the scorer; `FrequencyCandidate { offset_hz, score, clear_both_slots, noise_floor }`.

---

## Phase 1 — Selection foundation (PR 1, branch `feat/tui-selection-foundation`)

### Task 1: Callsign-pin the Band Activity cursor

**Files:**
- Modify: `pancetta-tui/src/app.rs` (field near `dx_hunter_pinned_call` ~line 681; methods near `clamp_dx_hunter_selection`; `scroll_up`/`scroll_down` BandActivity arms ~1178-1226; `add_decoded_message` ~1000)
- Test: `pancetta-tui/src/app.rs` tests module

**Interfaces:**
- Consumes: `App::displayed_messages() -> Vec<&DecodedMessageView>` (app.rs:1862 — directed-at-us pinned first, then newest-first; THE display order).
- Produces: `App.band_activity_pinned_call: Option<String>`, `fn clamp_band_activity_selection(&mut self)`, `fn pin_band_activity_selection(&mut self)`. Task 2 reads the pinned call.

- [ ] **Step 1: Write the failing test** (in app.rs `mod tests`, using the existing `App::new(Config::default(), None).await.unwrap()` + a local fixture helper):

```rust
fn ba_fixture(call: &str, directed: bool) -> DecodedMessageView {
    DecodedMessageView {
        timestamp: Utc::now(),
        frequency: 14.074,
        mode: "FT8".into(),
        snr: -10,
        delta_time: 0.2,
        delta_freq: 1500.0,
        call_sign: Some(call.to_string()),
        grid_square: None,
        message: format!("CQ {call} EM12"),
        distance: None,
        bearing: None,
        slot_parity: None,
        is_directed_at_us: directed,
        worked_before: false,
        needed: false,
        atno: false,
        priority_score: 0,
    }
}
// NOTE: if DecodedMessageView has fields not listed here, initialize them
// with Default-ish values to compile — check app.rs:45.

#[tokio::test]
async fn band_activity_cursor_follows_callsign_across_reorder() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.active_panel = ActivePanel::BandActivity;
    app.add_decoded_message(ba_fixture("K1AAA", false)).await.unwrap();
    app.add_decoded_message(ba_fixture("K2BBB", false)).await.unwrap();
    // Newest-first display: [K2BBB, K1AAA]. Move cursor to K1AAA (row 1) and pin.
    app.scroll_down();
    app.pin_band_activity_selection();
    assert_eq!(app.get_selected_station().as_deref(), Some("K1AAA"));

    // A new decode lands (K3CCC becomes row 0) AND a directed-at-us decode
    // pins to the very top — both shift K1AAA's index.
    app.add_decoded_message(ba_fixture("K3CCC", false)).await.unwrap();
    app.add_decoded_message(ba_fixture("K4DDD", true)).await.unwrap();

    // The cursor must still resolve to K1AAA, not whatever occupies row 1 now.
    assert_eq!(app.get_selected_station().as_deref(), Some("K1AAA"));
}

#[tokio::test]
async fn band_activity_pin_degrades_gracefully_when_station_ages_out() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.active_panel = ActivePanel::BandActivity;
    app.add_decoded_message(ba_fixture("K1AAA", false)).await.unwrap();
    app.pin_band_activity_selection();
    app.clear_messages();
    // Pinned call gone: index clamps to 0, pin cleared, no panic.
    assert_eq!(app.band_activity_scroll, 0);
}
```

(`get_selected_station` exists and walks `displayed_messages()` — if its exact name differs, grep `fn get_selected_station` in app.rs and use that.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p pancetta-tui --lib band_activity_cursor_follows -- --nocapture`
Expected: FAIL — `pin_band_activity_selection` not found.

- [ ] **Step 3: Implement.** Field (next to `dx_hunter_pinned_call`):

```rust
/// The callsign the Band Activity cursor is pinned to. The list reorders
/// on EVERY decode window (directed-at-us pinned first, then newest-first),
/// so a bare positional index silently retargets Space onto a different
/// station. Same pattern as `dx_hunter_pinned_call` / `qso_pinned_id`.
/// `None` = not yet pinned (track whatever is at `band_activity_scroll`).
band_activity_pinned_call: Option<String>,
```

Init `band_activity_pinned_call: None` in the `Self { .. }` constructor literal (next to `band_activity_scroll: 0`). Methods (place next to `clamp_dx_hunter_selection`, mirroring its shape):

```rust
/// Pin the Band Activity selection to the callsign currently under the
/// cursor. Call after every deliberate cursor move.
pub fn pin_band_activity_selection(&mut self) {
    self.band_activity_pinned_call = self
        .displayed_messages()
        .get(self.band_activity_scroll)
        .and_then(|m| m.call_sign.clone());
}

/// Re-derive `band_activity_scroll` from the pinned callsign after any
/// list mutation. Falls back to a clamp (and clears the pin) if the
/// pinned callsign left the list.
pub fn clamp_band_activity_selection(&mut self) {
    let displayed = self.displayed_messages();
    if let Some(ref pin) = self.band_activity_pinned_call {
        if let Some(idx) = displayed.iter().position(|m| {
            m.call_sign
                .as_deref()
                .is_some_and(|c| pancetta_core::callsign::callsigns_match(c, pin))
        }) {
            self.band_activity_scroll = idx;
            return;
        }
        self.band_activity_pinned_call = None;
    }
    let max = displayed.len().saturating_sub(1);
    self.band_activity_scroll = self.band_activity_scroll.min(max);
}
```

**Until Task 2 lands** `pancetta_core::callsign` does not exist — use `c.eq_ignore_ascii_case(pin)` here and switch to `callsigns_match` in Task 2 Step 4. Wire the calls: in `scroll_up`/`scroll_down`'s `ActivePanel::BandActivity` arms, call `self.pin_band_activity_selection()` after moving; at the end of `add_decoded_message` and in `clear_messages`, call `self.clamp_band_activity_selection()`. Also pin in the Home/End (`<`/`>`) jump handlers for BandActivity (tui_runner.rs:1037-1043 route into App methods — find where they set `band_activity_scroll` and pin after).

- [ ] **Step 4: Run tests**

Run: `cargo test -p pancetta-tui --lib band_activity -- --nocapture`
Expected: both new tests PASS; then full `cargo test -p pancetta-tui --lib` — all green.

- [ ] **Step 5: Commit** — `git add -A && git commit -m "feat(tui): callsign-pin the Band Activity cursor (kills the moving-target selection)"`

### Task 2: Move callsign equivalence to pancetta-core + global focus model

**Files:**
- Create: `pancetta-core/src/callsign.rs`
- Modify: `pancetta-core/src/lib.rs` (module + re-export), `pancetta-qso/src/exchange.rs` (delegate to core, keep `pub use` back-compat), `pancetta-tui/src/app.rs` (focus accessor + Task-1 switch to `callsigns_match`)
- Test: move/duplicate the existing `base_callsign` unit tests into `pancetta-core/src/callsign.rs`

**Interfaces:**
- Consumes: `base_callsign`/`callsigns_match` currently in `pancetta-qso/src/exchange.rs` (pure `fn(&str) -> ...`, no deps).
- Produces: `pancetta_core::callsign::{base_callsign, callsigns_match}`; `App::focused_callsign(&self) -> Option<String>`; `App::is_focused(&self, call: &str) -> bool`. Tasks 3, 11, 17, 19 consume these.

- [ ] **Step 1: Move the functions.** Cut `base_callsign` and `callsigns_match` (and ONLY those two + their doc comments) from `pancetta-qso/src/exchange.rs` into new `pancetta-core/src/callsign.rs` with module doc `//! Compound-callsign equivalence (catalog C18) — pure string helpers shared by the QSO engine and the TUI.`. `pancetta-core` enforces `#![warn(missing_docs)]` — keep the existing doc comments on both `pub fn`s. In lib.rs add `pub mod callsign;`. In exchange.rs replace the bodies with `pub use pancetta_core::callsign::{base_callsign, callsigns_match};` so every existing pancetta-qso caller and test keeps compiling unchanged. Move their unit tests to the new module (leave exchange.rs integration tests untouched — they exercise the re-export).

- [ ] **Step 2: Verify no behavior change**

Run: `cargo test -p pancetta-core -p pancetta-qso --features transmit 2>&1 | grep -E "test result|FAILED"`
Expected: all green (the compound-call adversarial suite `pancetta-qso/tests/adversarial_compound_calls.rs` is the real oracle).

- [ ] **Step 3: Write the failing focus test** (app.rs tests):

```rust
#[tokio::test]
async fn focused_callsign_follows_the_active_panel() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.add_decoded_message(ba_fixture("K1AAA", false)).await.unwrap();
    app.active_panel = ActivePanel::BandActivity;
    app.pin_band_activity_selection();
    assert_eq!(app.focused_callsign().as_deref(), Some("K1AAA"));
    // Compound equivalence: EA8/K1AAA is the same focus target.
    assert!(app.is_focused("EA8/K1AAA"));
    assert!(!app.is_focused("K1AAB"));
}
```

- [ ] **Step 4: Implement** (app.rs, near the pin methods):

```rust
/// The station under the operator's attention: the ACTIVE panel's pinned
/// (or cursor-resolved) callsign. One focus, shared by every panel —
/// renderers highlight it everywhere via `is_focused`.
pub fn focused_callsign(&self) -> Option<String> {
    match self.active_panel {
        ActivePanel::BandActivity => self
            .band_activity_pinned_call
            .clone()
            .or_else(|| self.get_selected_station()),
        ActivePanel::DxHunter => self.dx_hunter_pinned_call.clone().or_else(|| {
            self.sorted_dx_stations()
                .get(self.dx_hunter_scroll)
                .map(|s| s.callsign.clone())
        }),
        ActivePanel::Callers => self.callers_pinned_call.clone().or_else(|| {
            self.displayed_callers()
                .get(self.callers_scroll)
                .and_then(|m| m.call_sign.clone())
        }),
        ActivePanel::QsoStatus => self
            .active_qsos
            .get(self.qso_cursor)
            .map(|q| q.their_callsign.clone()),
        ActivePanel::StationInfo => None,
    }
}

/// Compound-aware "is this callsign the current focus?"
pub fn is_focused(&self, call: &str) -> bool {
    self.focused_callsign()
        .is_some_and(|f| pancetta_core::callsign::callsigns_match(&f, call))
}
```

(`sorted_dx_stations` — grep for the DX-Hunter renderer's list source; use the SAME method the renderer walks so focus and highlight agree. If it's named differently, adapt.) Also switch Task 1's `eq_ignore_ascii_case` to `callsigns_match` now. Then add the cross-panel highlight: in `band_activity.rs::create_message_row`, `dx_hunter.rs`'s row builder, and `callers.rs::caller_row`, when `app.is_focused(<row callsign>)` and the row's panel is NOT the active one, style the callsign cell `Style::default().fg(app.theme.selected_color()).add_modifier(Modifier::BOLD)` (the active panel keeps its existing REVERSED row-highlight — no double treatment).

- [ ] **Step 5: Run tests, then full suite**

Run: `cargo test -p pancetta-tui --lib` → all green (164 + new).

- [ ] **Step 6: Commit** — `"feat(tui): global station focus — one focus shared across panels, compound-aware (helpers moved to pancetta-core)"`

### Task 3: Engaged-set secondary highlight (multi-TX tier 2)

**Files:**
- Modify: `pancetta-tui/src/app.rs` (accessor), `ui/band_activity.rs`, `ui/dx_hunter.rs`, `ui/callers.rs`
- Test: app.rs tests

**Interfaces:**
- Consumes: `App.active_qsos: Vec<ActiveQsoBanner>` (`their_callsign`, `frequency_hz` fields — app.rs:106).
- Produces: `App::is_engaged(&self, call: &str) -> bool`. Task 11 (strip markers) and Task 17 (card) also consume `active_qsos` directly.

- [ ] **Step 1: Failing test**

```rust
#[tokio::test]
async fn engaged_calls_are_flagged_compound_aware() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.active_qsos.push(fixture_banner("JA1ABC", "wait rpt", None)); // existing test helper
    assert!(app.is_engaged("JA1ABC"));
    assert!(app.is_engaged("JA1ABC/P"));
    assert!(!app.is_engaged("JA1ABD"));
}
```

- [ ] **Step 2: Implement**

```rust
/// Is this callsign one of our current active-QSO partners (an "engaged"
/// stream)? Engaged stations get the tier-2 highlight (green ● + underline)
/// in every list — visible even while the focus is elsewhere (multi-TX).
pub fn is_engaged(&self, call: &str) -> bool {
    self.active_qsos
        .iter()
        .any(|q| pancetta_core::callsign::callsigns_match(&q.their_callsign, call))
}
```

Renderers: in each of the three row builders, when `app.is_engaged(call)`: prefix the callsign string with `"● "` and add `Modifier::UNDERLINED` + `fg(app.theme.success_color())` to the callsign cell (directed-at-us styling still wins in band_activity — apply engaged style only when `!msg.is_directed_at_us`). Keep Band Activity's `Call` column `Constraint::Length(8)` → widen to `Length(10)` to absorb the prefix.

- [ ] **Step 3: Space acts on the global focus (spec §4).** `resolve_space_action` (tui_runner.rs:1415 comment block) currently resolves the target from the active panel's own selection. Repoint its station source to `app.focused_callsign()` so Space works identically from ANY panel — the per-panel context-resolution of WHICH reply step to send stays exactly as-is (it already keys off the station's last message, not the panel). Test: pin focus to a CQing station from the DX Hunter panel, simulate Space, assert the captured `TuiCommand` targets that callsign (copy the existing Space key-test harness).

- [ ] **Step 4: Tests + full suite green.** `cargo test -p pancetta-tui --lib`

- [ ] **Step 5: Phase gate + commit.** Run `cargo fmt --all && cargo clippy --all-targets -p pancetta-tui -p pancetta-core -p pancetta-qso && cargo test --workspace --features transmit --exclude pancetta-research`. Commit `"feat(tui): engaged-stream tier-2 highlight + Space-on-focus (multi-TX visible while focus is elsewhere)"`. **Phase 1 / PR 1 complete.**

---

## Phase 2 — View scaffold + zoom (PR 2, branch `feat/tui-activity-views`)

### Task 4: `ActiveView` enum, v/V cycling, persistence, title chip

**Files:**
- Create: `pancetta-tui/src/view.rs`
- Modify: `pancetta-tui/src/lib.rs` (`pub mod view;`), `app.rs` (field + load/save), `tui_runner.rs` (keys `v`/`V` in the main `match key.code` — both unbound today), `ui/mod.rs::render_title_bar` (view chip)
- Test: `view.rs` unit tests + one tui_runner key test (copy the pattern of the existing key tests at tui_runner.rs:1978+)

**Interfaces:**
- Produces: `pub enum ActiveView { Operate, Hunt, Run, Monitor }` with `next()`, `prev()`, `label() -> Option<&'static str>` (None for Operate), `from_str`/`as_str` for persistence; `App.active_view: ActiveView`; `App::cycle_view(forward: bool)` (persists best-effort). Tasks 5-7, 11, 17, 19 consume `app.active_view`.

- [ ] **Step 1: Failing tests** (view.rs):

```rust
#[test]
fn view_cycle_is_a_4_ring() {
    use ActiveView::*;
    assert_eq!(Operate.next(), Hunt);
    assert_eq!(Monitor.next(), Operate);
    assert_eq!(Operate.prev(), Monitor);
}
#[test]
fn view_label_hidden_for_operate() {
    assert_eq!(ActiveView::Operate.label(), None);
    assert_eq!(ActiveView::Hunt.label(), Some("HUNT"));
    assert_eq!(ActiveView::Run.label(), Some("RUN"));
    assert_eq!(ActiveView::Monitor.label(), Some("MON"));
}
#[test]
fn view_persistence_round_trip() {
    for v in [ActiveView::Operate, ActiveView::Hunt, ActiveView::Run, ActiveView::Monitor] {
        assert_eq!(ActiveView::from_str_or_default(v.as_str()), v);
    }
    assert_eq!(ActiveView::from_str_or_default("garbage"), ActiveView::Operate);
}
```

- [ ] **Step 2: Implement view.rs** (plain enum + the four methods, `#[derive(Debug, Clone, Copy, PartialEq, Eq)]`), then App wiring:

```rust
// app.rs — persistence (best-effort, never fails the TUI):
fn tui_state_path() -> Option<std::path::PathBuf> {
    dirs::home_dir().map(|h| h.join(".pancetta").join("tui_state.json"))
}
pub fn load_persisted_view() -> crate::view::ActiveView {
    tui_state_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        .and_then(|v| v.get("active_view").and_then(|x| x.as_str()).map(String::from))
        .map(|s| crate::view::ActiveView::from_str_or_default(&s))
        .unwrap_or(crate::view::ActiveView::Operate)
}
pub fn cycle_view(&mut self, forward: bool) {
    self.active_view = if forward { self.active_view.next() } else { self.active_view.prev() };
    if let Some(p) = Self::tui_state_path() {
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let _ = std::fs::write(&p, format!("{{\"active_view\":\"{}\"}}", self.active_view.as_str()));
    }
    self.status_message = format!("View: {:?}", self.active_view);
}
```

`App::new` initializes `active_view: Self::load_persisted_view()`. (`serde_json` — check pancetta-tui/Cargo.toml; add `serde_json = "1"` if absent.) Keys in tui_runner main match: `KeyCode::Char('v') => app.cycle_view(true)`, `KeyCode::Char('V') => app.cycle_view(false)`. Title chip in `render_title_bar` (after the mode chip block, ui/mod.rs:243-252): if `let Some(label) = app.active_view.label()` push a `" {label} "` span styled `fg(Black) bg(Color::White) BOLD`. Add the `v`/`V` line to the `?` help overlay list (tui_runner.rs:1685+) and the always-visible hint line is full — leave it.

**Suggest-never-auto-switch (spec §1):** in the `TuiMessage::FoxModeUpdate` handler (tui_runner.rs:~665), when `on == true && app.active_view != ActiveView::Run`, set `app.status_message = "FOX on — press v for Run view".to_string()` (a hint, NOT a view change). Test: apply FoxModeUpdate{on:true} with Operate active → view unchanged, status contains "press v".

- [ ] **Step 3: Tests + suite green; commit** `"feat(tui): ActiveView enum + v/V cycling + persistence + title chip (all views still render Operate layout)"`

### Task 5: Per-view layout dispatch + Monitor view

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs` (`draw()` — extract lines ~90-159 into `layout_operate`; add `layout_monitor`; dispatch on `app.active_view`)
- Test: new `ui/mod.rs` tests module using `ratatui::backend::TestBackend`

**Interfaces:**
- Consumes: `app.active_view`, all existing `render_*` fns.
- Produces: `fn layout_operate(f, area, app) -> Result<()>`, `fn layout_monitor(f, area, app) -> Result<()>`. Task 6 adds `layout_hunt`/`layout_run` beside them; Task 7's zoom bypasses them.

- [ ] **Step 1: Failing TestBackend smoke test**:

```rust
#[cfg(test)]
mod view_render_tests {
    use super::*;
    use ratatui::{backend::TestBackend, Terminal};

    async fn render_view(view: crate::view::ActiveView) -> ratatui::buffer::Buffer {
        let mut app = crate::app::App::new(pancetta_config::Config::default(), None)
            .await
            .unwrap();
        app.active_view = view;
        let backend = TestBackend::new(120, 40);
        let mut term = Terminal::new(backend).unwrap();
        term.draw(|f| draw(f, &app).unwrap()).unwrap();
        term.backend().buffer().clone()
    }
    fn buffer_contains(buf: &ratatui::buffer::Buffer, needle: &str) -> bool {
        (0..buf.area.height).any(|y| {
            let row: String = (0..buf.area.width)
                .map(|x| buf[(x, y)].symbol().chars().next().unwrap_or(' '))
                .collect();
            row.contains(needle)
        })
    }

    #[tokio::test]
    async fn operate_view_shows_all_five_panels() {
        let buf = render_view(crate::view::ActiveView::Operate).await;
        for t in ["Band Activity", "QSO Status", "DX Hunter", "Callers"] {
            assert!(buffer_contains(&buf, t), "missing {t}");
        }
    }
    #[tokio::test]
    async fn monitor_view_drops_side_panels() {
        let buf = render_view(crate::view::ActiveView::Monitor).await;
        assert!(buffer_contains(&buf, "Band Activity"));
        assert!(!buffer_contains(&buf, "DX Hunter"));
        assert!(!buffer_contains(&buf, "Callers"));
    }
}
```

- [ ] **Step 2: Implement.** Move the existing content-layout block (banner + waterfall + lower 2-column grid + panel-highlight call, ui/mod.rs:90-159) verbatim into `fn layout_operate(f: &mut Frame<'_>, content_area: Rect, app: &App) -> Result<()>`; `draw()` keeps title/TX-strip/status-bar and dispatches:

```rust
match app.active_view {
    crate::view::ActiveView::Operate => layout_operate(f, chunks[1], app)?,
    crate::view::ActiveView::Monitor => layout_monitor(f, chunks[1], app)?,
    // Hunt/Run land in Task 6 — until then they render Operate:
    _ => layout_operate(f, chunks[1], app)?,
}
```

`layout_monitor`: vertical `[Length(1) banner, Percentage(60) waterfall, Min(1) band activity full width]`; call `render_active_qsos`, `render_waterfall`, `render_band_activity`; panel-highlight slice only for BandActivity (pass the one rect; guard `render_active_panel_highlight` against missing panels — simplest: in Monitor, skip the highlight call and let the Band Activity block's `create_panel_block` active-border suffice).

- [ ] **Step 3: Tests green (incl. full 164); commit** `"feat(tui): per-view layout dispatch + Monitor view"`

### Task 6: Hunt + Run layouts

**Files:**
- Modify: `ui/mod.rs` (`layout_hunt`, `layout_run`), `app.rs` (`displayed_messages` view-aware CQ filter)
- Test: extend `view_render_tests` + a filter unit test

**Interfaces:**
- Consumes: `layout_operate` pattern, `render_*` fns, `app.active_view`.
- Produces: Hunt's CQs-only rule inside `displayed_messages()` (so cursor, renderer, and Space all agree — this is load-bearing).

- [ ] **Step 1: Failing filter test:**

```rust
#[tokio::test]
async fn hunt_view_filters_band_activity_to_cqs_and_directed() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.add_decoded_message(ba_fixture("K1AAA", false)).await.unwrap(); // "CQ K1AAA EM12"
    let mut non_cq = ba_fixture("K2BBB", false);
    non_cq.message = "K9ZZZ K2BBB -10".into();
    app.add_decoded_message(non_cq).await.unwrap();
    let mut directed = ba_fixture("K3CCC", true);
    directed.message = "K5ARH K3CCC RR73".into();
    app.add_decoded_message(directed).await.unwrap();

    app.active_view = crate::view::ActiveView::Hunt;
    let shown: Vec<_> = app.displayed_messages().iter().filter_map(|m| m.call_sign.clone()).collect();
    assert!(shown.contains(&"K1AAA".to_string()), "CQ kept");
    assert!(shown.contains(&"K3CCC".to_string()), "directed-at-us always kept");
    assert!(!shown.contains(&"K2BBB".to_string()), "third-party exchange filtered");
}
```

- [ ] **Step 2: Implement.** In `displayed_messages()` (app.rs:1862), after building the two vecs, when `self.active_view == ActiveView::Hunt` retain in `others` only rows where `m.message.trim_start().starts_with("CQ")` (directed vec always kept). `layout_hunt`: vertical `[Length(1) banner, Percentage(45) DX Hunter full width, Percentage(35) band activity, Min(5) QSO status]`. `layout_run`: `[Length(1) banner, Percentage(50) callers full width, Min(5) QSO status]` (QSO Status's existing multi-table mode is the "Active QSOs" table). Wire both into the `draw()` dispatch. Extend the smoke tests: Hunt shows "DX Hunter" + "Band Activity", no "Callers"; Run shows "Callers" + "QSO Status", no "DX Hunter".

- [ ] **Step 3: Tests green; commit** `"feat(tui): Hunt and Run view layouts (CQs-only band activity in Hunt)"`

### Task 7: `z` panel zoom

**Files:**
- Modify: `app.rs` (`pub zoomed: bool`, init false), `tui_runner.rs` (key `z` — unbound today; Esc clears; help entry), `ui/mod.rs` (`draw()` zoom branch + `fn render_zoomed_panel`)
- Test: `view_render_tests`

**Interfaces:**
- Produces: `app.zoomed`; zoom branch in `draw()` that Task 13 extends for the placement top-10.

- [ ] **Step 1: Failing test:**

```rust
#[tokio::test]
async fn zoom_renders_only_the_focused_panel() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.active_panel = ActivePanel::DxHunter;
    app.zoomed = true;
    let backend = TestBackend::new(120, 40);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &app).unwrap()).unwrap();
    let buf = term.backend().buffer().clone();
    assert!(buffer_contains(&buf, "DX Hunter"));
    assert!(!buffer_contains(&buf, "Band Activity"));
}
```

- [ ] **Step 2: Implement.** In `draw()` before the view dispatch: `if app.zoomed { return render_zoomed_panel(f, chunks[1], app); }` — a match on `app.active_panel` calling that panel's existing `render_*` with the whole content rect. Keys: `KeyCode::Char('z') => app.zoomed = !app.zoomed;`; in the existing `KeyCode::Esc` arm chain add `else if app.zoomed { app.zoomed = false; }` (AFTER the stopped-banner arm, tui_runner.rs:992); Tab clears zoom before switching (`app.zoomed = false;` at the top of the Tab arm). Help overlay entry: `("z", "Zoom focused panel (again/Esc to restore)")`.

- [ ] **Step 3: Tests + full suite; phase gate** (fmt/clippy/workspace test). Commit `"feat(tui): z zooms the focused panel"`. **Phase 2 / PR 2 complete.**

---

## Phase 3 — TX-placement instrument (PR 3, branch `feat/tx-placement-instrument`)

### Task 8: Per-slot clear flags on FrequencyCandidate (pancetta-qso)

**Files:**
- Modify: `pancetta-qso/src/frequency.rs` (`FrequencyCandidate` + `rank_candidates`)
- Test: frequency.rs tests module

**Interfaces:**
- Produces: `FrequencyCandidate { offset_hz, score, clear_both_slots, clear_first, clear_second, noise_floor }` — two new `pub bool` fields. Task 9 serializes them. (`TimeSlot::{First, Second}` — First = even slot.) Existing constructors of `FrequencyCandidate` in tests must gain the fields.

- [ ] **Step 1: Failing test:**

```rust
#[test]
fn candidates_carry_per_slot_clear_flags() {
    let alloc = SmartFrequencyAllocator::new(FrequencyAllocatorConfig::default());
    let spectral = SpectralSnapshot { power_bins: vec![0.0; 128], freq_min_hz: 200.0, freq_max_hz: 3000.0 };
    let mut history = DecodeHistory::new(4);
    // 1500 Hz busy in First slot only.
    history.push_cycle(vec![DecodeRecord { frequency_hz: 1500.0, time_slot: TimeSlot::First }]);
    let cands = alloc.rank_candidates(&spectral, &history, &[], None);
    let at_1500 = cands.iter().find(|c| (c.offset_hz - 1500.0).abs() < 1.0).unwrap();
    assert!(!at_1500.clear_first);
    assert!(at_1500.clear_second);
    assert!(!at_1500.clear_both_slots);
    let far = cands.iter().find(|c| (c.offset_hz - 700.0).abs() < 1.0).unwrap();
    assert!(far.clear_first && far.clear_second && far.clear_both_slots);
}
```

- [ ] **Step 2: Implement.** Add fields with docs (`/// No decode activity within 50 Hz in the First (even) slot across retained history.`). In `rank_candidates`'s candidate-construction loop add:

```rust
let clear_first = history.activity_near_in_slot(freq, 50.0, TimeSlot::First) == 0;
let clear_second = history.activity_near_in_slot(freq, 50.0, TimeSlot::Second) == 0;
```

Fix any struct-literal compile errors in existing tests by adding the two fields (values consistent with their history fixtures).

- [ ] **Step 3:** `cargo test -p pancetta-qso --lib frequency` green + full `-p pancetta-qso`; commit `"feat(qso): per-slot clear flags on FrequencyCandidate (TX-placement instrument feed)"`

### Task 9: Placement computation + `TxPlacementUpdate` bus message (coordinator)

**Files:**
- Modify: `pancetta/src/message_bus.rs` (variant + structs), `pancetta-qso/src/autonomous.rs` (public ranking accessor), `pancetta/src/coordinator/autonomous.rs` (per-slot compute + send, in the :451-493 tick right after `op.feed_decoded_messages`)
- Test: autonomous.rs (qso crate) unit test + a coordinator pure-fn test

**Interfaces:**
- Consumes: `AutonomousOperator { smart_allocator, spectral_snapshot, decode_history, frequency_allocator.own_frequencies() }` (all private — hence the new method), `FrequencyCandidate` (Task 8).
- Produces:
  - `pancetta-qso`: `pub fn AutonomousOperator::placement_snapshot(&self, top_n: usize) -> Option<PlacementSnapshot>` and `pub struct PlacementSnapshot { pub slices: Vec<FrequencyCandidate>, pub openness: Vec<u8>, pub bin_hz: f64, pub range: (f64, f64) }` (in frequency.rs; openness code: 0=busy-both, 1=second-only-clear, 2=first-only-clear, 3=clear-both).
  - `pancetta` bus: `MessageType::TxPlacementUpdate { snapshot: pancetta_qso::frequency::PlacementSnapshot }` (derive `Clone`+`Debug` on the snapshot).
  Task 10 relays it; Task 16 reuses `placement_snapshot`.

- [ ] **Step 1: Failing test** (frequency.rs or autonomous.rs tests — wherever `AutonomousOperator` test constructors already exist; grep `AutonomousOperator::new` in pancetta-qso tests for the fixture pattern):

```rust
#[test]
fn placement_snapshot_ranks_and_bins() {
    // Build the operator fixture the same way existing autonomous tests do,
    // then feed spectral + one busy decode and ask for a snapshot.
    let mut op = test_operator(); // existing helper — grep `fn test_operator` / nearest equivalent
    op.update_spectral(SpectralSnapshot { power_bins: vec![0.0; 128], freq_min_hz: 200.0, freq_max_hz: 3000.0 });
    op.decode_history_mut_for_test().push_cycle(vec![DecodeRecord { frequency_hz: 1500.0, time_slot: TimeSlot::First }]);
    let snap = op.placement_snapshot(10).unwrap();
    assert_eq!(snap.slices.len(), 10);
    assert!(snap.slices.windows(2).all(|w| w[0].score >= w[1].score), "sorted by score desc");
    let bin_1500 = ((1500.0 - snap.range.0) / snap.bin_hz) as usize;
    assert_eq!(snap.openness[bin_1500], 1, "busy in First → second-only-clear");
}
```

(If no `decode_history_mut_for_test` exists, add `#[cfg(test)] pub(crate) fn decode_history_mut_for_test(&mut self) -> &mut DecodeHistory`.)

- [ ] **Step 2: Implement `placement_snapshot`** (autonomous.rs, next to `allocate_smart_frequency`):

```rust
/// Rank the current band openness for the TX-placement instrument.
/// Returns None until the first spectral snapshot arrives. Pure read —
/// does NOT allocate or mutate; the SAME allocator/history the autonomous
/// path uses (single-scorer invariant).
pub fn placement_snapshot(&self, top_n: usize) -> Option<PlacementSnapshot> {
    let spectral = self.spectral_snapshot.as_ref()?;
    let own: Vec<f64> = self.frequency_allocator.own_frequencies().values().copied().collect();
    let mut cands = self.smart_allocator.rank_candidates(spectral, &self.decode_history, &own, None);
    cands.truncate(top_n);
    let (min_f, max_f) = self.smart_allocator.config().range;
    let bin_hz = self.smart_allocator.config().step_hz;
    let bins = ((max_f - min_f) / bin_hz).ceil() as usize;
    let openness = (0..bins)
        .map(|i| {
            let f = min_f + i as f64 * bin_hz;
            let cf = self.decode_history.activity_near_in_slot(f, 50.0, TimeSlot::First) == 0;
            let cs = self.decode_history.activity_near_in_slot(f, 50.0, TimeSlot::Second) == 0;
            match (cf, cs) { (true, true) => 3u8, (true, false) => 2, (false, true) => 1, (false, false) => 0 }
        })
        .collect();
    Some(PlacementSnapshot { slices: cands, openness, bin_hz, range: (min_f, max_f) })
}
```

(`rank_candidates` already sorts desc — verify at frequency.rs:190-224; if not, sort here. `config()` accessor: add `pub fn config(&self) -> &FrequencyAllocatorConfig` to `SmartFrequencyAllocator` if private.) Bus variant in message_bus.rs after `DiagnosticEvent`:

```rust
/// Per-window TX-placement ranking for the TUI instrument
/// (docs/superpowers/specs/2026-07-03-tui-redesign-design.md §2). Computed
/// in the autonomous tick from the SAME SmartFrequencyAllocator the
/// autonomous path allocates with — the TUI never re-derives scores.
TxPlacementUpdate { snapshot: pancetta_qso::frequency::PlacementSnapshot },
```

Coordinator send (coordinator/autonomous.rs, immediately after `op.feed_decoded_messages(...)` at :480, while `op` is locked):

```rust
if let Some(snapshot) = op.placement_snapshot(10) {
    let msg = ComponentMessage::new(
        ComponentId::Autonomous,
        ComponentId::Tui,
        MessageType::TxPlacementUpdate { snapshot },
        Instant::now(),
    );
    let _ = bus_for_decisions.send_message(msg).await;
}
```

(Use whatever bus handle that block already has in scope — grep the surrounding sends.)

- [ ] **Step 3:** `cargo test -p pancetta-qso --features transmit` + `cargo build -p pancetta` green; commit `"feat(coord): per-window TxPlacementUpdate from the autonomous tick's allocator (single-scorer invariant)"`

### Task 10: Relay + App placement state

**Files:**
- Modify: `pancetta/src/coordinator/tui_relay.rs` (arm beside the `DiagnosticEvent` arm, :499-540 region), `pancetta-tui/src/tui_runner.rs` (`TuiMessage::TxPlacementUpdate` + handle arm), `pancetta-tui/src/app.rs` (state)
- Test: app.rs test

**Interfaces:**
- Consumes: `MessageType::TxPlacementUpdate` (Task 9). **`pancetta-tui` must not depend on `pancetta-qso`** — the relay converts to a TUI-local mirror.
- Produces (app.rs, next to `DiagnosticEventRecord`):

```rust
#[derive(Debug, Clone)]
pub struct PlacementSlice {
    pub offset_hz: f64,
    pub score: f64,
    pub clear_first: bool,
    pub clear_second: bool,
}
#[derive(Debug, Clone)]
pub struct PlacementView {
    pub slices: Vec<PlacementSlice>,   // score-desc, ≤10
    pub openness: Vec<u8>,             // 0 busy, 1 second-only, 2 first-only, 3 both
    pub bin_hz: f64,
    pub range: (f64, f64),
    pub received_at: chrono::DateTime<chrono::Utc>,
}
```

plus `App.placement: Option<PlacementView>`, `App.placement_cursor: usize` (index into `slices`), `App.parked_since: Option<DateTime<Utc>>`. Tasks 11-16 consume all of these.

- [ ] **Step 1: Failing test:** push a `PlacementView` via the new `TuiMessage` handler path (call `app`'s handler directly or set the field through a small `pub fn apply_placement(&mut self, v: PlacementView)`) and assert `placement_cursor` clamps to `slices.len()-1` when the new snapshot is shorter.

```rust
#[tokio::test]
async fn placement_update_clamps_cursor() {
    let mut app = App::new(Config::default(), None).await.unwrap();
    app.placement_cursor = 7;
    app.apply_placement(PlacementView {
        slices: vec![PlacementSlice { offset_hz: 1480.0, score: 98.0, clear_first: true, clear_second: true }],
        openness: vec![3; 96], bin_hz: 25.0, range: (200.0, 2600.0),
        received_at: Utc::now(),
    });
    assert_eq!(app.placement_cursor, 0);
}
```

- [ ] **Step 2: Implement** `apply_placement` (set field, clamp cursor, stamp `received_at`), the `TuiMessage::TxPlacementUpdate { view: PlacementView }` variant + `handle_message` arm calling it, and the relay arm mapping field-for-field (`clear_first`/`clear_second` from the qso-side candidate).

- [ ] **Step 3:** tests + `cargo build -p pancetta` green; commit `"feat(tui): relay TxPlacementUpdate into App placement state"`

### Task 11: Instrument renderer (strip + BEST row + park line + stream markers)

**Files:**
- Create: `pancetta-tui/src/ui/tx_placement.rs`
- Modify: `ui/mod.rs` (module + swap into `layout_operate`/`layout_hunt`/`layout_run`; Monitor untouched), `app.rs` (`ActivePanel::TxPlacement` — NOTE the ripple list below)
- Test: TestBackend render test

**Interfaces:**
- Consumes: `app.placement`, `app.tx_offset_hold_hz: Option<u64>` (existing — title-bar chip reads it), `app.parked_since`, `app.active_qsos` (stream markers: `frequency_hz` + `their_callsign`), `app.focused_callsign()`.
- Produces: `pub fn render_tx_placement(f, area, app) -> Result<()>` (5 rows). `ActivePanel::TxPlacement` variant — **ripples to update:** `ActivePanel::next/prev` (insert after BandActivity), `render_active_panel_highlight`'s rect slice in each layout fn, `focused_callsign` (TxPlacement → None), the `1-5` jump keys stay as-is (placement gets no number key; Tab reaches it).

- [ ] **Step 1: Failing render test** (in the ui tests module): build an `App` with a canned `PlacementView` (3 slices: 1480 both-clear score 98, 920 both-clear 91, 310 second-only 71) + one banner via `let mut b = fixture_banner("JA1ABC", "wait rpt", None); b.frequency_hz = 1650.0;` (the helper defaults to 1234.0); render Operate at 120×40; assert `buffer_contains(&buf, "① 1480")`, `buffer_contains(&buf, "E+O")`, `buffer_contains(&buf, "JA1ABC")` (stream marker label), and `!buffer_contains(&buf, "▼")` (old waterfall decode ticks gone from Operate). Add two sibling tests per the spec's testing section: zero streams (no marker row content, no panic) and three streams (three distinct labels present).

- [ ] **Step 2: Implement `render_tx_placement`.** Five rows inside a `create_panel_block("TX Placement", is_active, app)` block:
  1. **Openness strip:** for each screen column, map to a bin (`range` + `bin_hz` scaled across width, same math as `Waterfall::freq_to_col`); glyph+color by code: 3 → `█` `success_color`, 2 → `▀` `warning_color`, 1 → `▄` `success_color` dimmed (`Color::Green` vs `LightGreen` distinction is theme-dependent — use `success_color` for 3, `warning_color` for 2 (First/even clear), `accent_color` for 1 (Second/odd clear), `muted_color` `·` for 0). Legend at line end when width allows: `█both ▀E ▄O`.
  2. **Stream-marker row:** for each active QSO banner, place `│` at its `frequency_hz` column (fg `error_color` if that bin's openness is 0, else `success_color`) with the callsign written after it while columns remain; the focused station (if it has a known freq from its latest decode — look up `app.decoded_messages` newest entry for the focus call) gets `◆` in `selected_color`.
  3. **BEST row:** `slices.iter().take(5).enumerate()` → `"{①..⑤} {offset:.0} {E+O|E|O} {score:.0}"`, cursor slice REVERSED. Coverage label: `clear_first && clear_second → "E+O"`, `clear_first → "E"`, `clear_second → "O"`.
  4. **Park line:** if `app.tx_offset_hold_hz` is Some(hz): `"parked: {hz} ({coverage-of-that-bin}, holding {mins} min)"` + degradation flag (Task 14). Else `"not parked — Enter parks ①"`. Trailing hint: `"←/→ pick · Enter=park · z=top-10"`.
  5. Frequency axis row (reuse the tick logic style from `widgets/mod.rs:340-367`, or omit ticks below width 60).

  Swap into the three layout fns: replace their waterfall region (`Percentage(30)`) with `Length(7)` (5 content + 2 border) calling `render_tx_placement`; give the freed rows to the largest table in each view. Monitor keeps `render_waterfall` untouched.

- [ ] **Step 3:** tests + full lib suite green (Operate smoke test from Task 5 must be updated: it now asserts "TX Placement" instead of the waterfall); commit `"feat(tui): TX-placement instrument replaces the waterfall in Operate/Hunt/Run"`

### Task 12: Park interactions

**Files:**
- Modify: `tui_runner.rs` (key handling for `ActivePanel::TxPlacement`), `app.rs` (cursor moves)
- Test: tui_runner key tests (existing pattern)

**Interfaces:**
- Consumes: `TuiCommand::SetTxOffset { offset_hz: Option<u64> }` (exists; relay at tui_relay.rs:1372 stores hz + flips `TxFreqMode::Hold`), `app.placement`, `app.placement_cursor`.
- Produces: Enter-on-TxPlacement parks; `app.parked_since` stamped.

- [ ] **Step 1: Failing test:** simulate `Left`/`Right` with `active_panel = TxPlacement` and a 3-slice placement → `placement_cursor` moves 0→1→0 and saturates; simulate `Enter` → captured `TuiCommand::SetTxOffset { offset_hz: Some(920) }` on the command channel (existing tests capture `message_tx` — copy that harness).

- [ ] **Step 2: Implement.** In the main key match, guard the existing `Left`/`Right` TX-offset-nudge arms (tui_runner.rs:1055-1078): when `app.active_panel == ActivePanel::TxPlacement`, move `placement_cursor` instead (`saturating_sub(1)` / `min(slices.len()-1)`). `Enter` arm: when TxPlacement is active and a slice exists at the cursor, send `TuiCommand::SetTxOffset { offset_hz: Some(slice.offset_hz.round() as u64) }`, set `app.parked_since = Some(Utc::now())`, status `"Parked at {hz} Hz ({coverage})"`. Help overlay: `("Enter", "TX Placement: park at selected slice")` appended to the existing Enter line.

  *Documented divergence from spec §2:* the spec sketches "`←/→` moves a frequency cursor" (free-Hz). This task implements ←/→ as **slice-stepping** over the ranked BEST list instead — the ranked slices ARE the instrument's point, free-Hz parking already exists via the `o` modal, and Task 19 adds click-anywhere parking. If the operator wants free-Hz keyboard tuning on the strip later, it's an additive follow-up.

- [ ] **Step 3:** tests green; commit `"feat(tui): park interactions — cursor + Enter parks via existing SetTxOffset path"`

### Task 13: Top-10 zoom panel

**Files:**
- Modify: `ui/tx_placement.rs` (`pub fn render_placement_zoom`), `ui/mod.rs::render_zoomed_panel` (TxPlacement arm)
- Test: TestBackend test

**Interfaces:**
- Consumes: Task 7's zoom branch, `app.placement`.
- Produces: full-screen table — columns `#, Freq, Windows, Score, Gap(Hz), Quiet` (Gap = distance to the nearest busy bin on each side × `bin_hz`; Quiet = `now - received_at` is NOT per-slice history — render `-` for Quiet in v1 and document it as a follow-up column; do NOT fake data).

- [ ] **Step 1: Failing test:** zoomed TxPlacement renders header `"Freq"` + 10 rows (canned 10-slice placement) + row `"① 1480"`.

- [ ] **Step 2: Implement** the table (ratatui `Table`, same style constants as dx_hunter) + zoom dispatch arm.

- [ ] **Step 3:** tests green; commit `"feat(tui): top-10 placement zoom table"`

### Task 14: Parked-slice degradation warning

**Files:**
- Modify: `app.rs` (`apply_placement` edge-detect), `ui/tx_placement.rs` (park-line flag)
- Test: app.rs test

**Interfaces:**
- Consumes: `apply_placement` (Task 10), parked offset, openness codes.
- Produces: `App.park_coverage_last: Option<u8>`; on a Both→worse transition sets `status_message = "⚠ parked {hz} now busy in {E|O|both} — ① {best} better"` (one-shot per transition, not per update).

- [ ] **Step 1: Failing test:** park at a both-clear bin, `apply_placement` with that bin now code 1 → status_message contains `"now busy"`; apply again unchanged → status_message NOT rewritten (stash a sentinel first).

- [ ] **Step 2: Implement** in `apply_placement`: look up the parked offset's bin code; compare to `park_coverage_last`; on strict decrease from 3 set the warning + always update `park_coverage_last`. Park line renders `⚠` + current coverage whenever code < 3.

- [ ] **Step 3:** tests green; commit `"feat(tui): parked-slice degradation warning (edge-triggered)"`

### Task 15: Coordinator-side placement DiagnosticEvent on degradation

**Files:**
- Modify: `pancetta/src/coordinator/autonomous.rs` (after the Task-9 send)
- Test: pure-fn test in the same file

**Interfaces:**
- Consumes: snapshot + the coordinator's `tx_offset_hold_hz: Arc<AtomicU64>` — **thread it into the autonomous task** (add a param to `start_autonomous_component`, cloned from the coordinator field; grep `tx_offset_hold_hz` in coordinator/mod.rs for the owner).
- Produces: a `MessageType::DiagnosticEvent { target: "tx.placement", level: Warn, .. }` when the parked slice's openness drops below 3 (edge-triggered via a local `last_coverage: Option<u8>` in the task loop) — so the retained Shift+D history records it even if the operator missed the status blip.

- [ ] **Step 1: Write pure helper + failing test:**

```rust
/// (parked_hz, snapshot) -> openness code at the parked bin, if parked+known.
fn parked_bin_coverage(parked_hz: u64, snap: &pancetta_qso::frequency::PlacementSnapshot) -> Option<u8> {
    if parked_hz == 0 { return None; }
    let idx = ((parked_hz as f64 - snap.range.0) / snap.bin_hz) as usize;
    snap.openness.get(idx).copied()
}

#[test]
fn parked_bin_coverage_maps_hz_to_bin() {
    let snap = pancetta_qso::frequency::PlacementSnapshot {
        slices: vec![], openness: vec![3, 1, 0], bin_hz: 25.0, range: (200.0, 275.0),
    };
    assert_eq!(parked_bin_coverage(0, &snap), None);
    assert_eq!(parked_bin_coverage(225, &snap), Some(1));
}
```

- [ ] **Step 2: Implement** the loop-side edge detect + DiagnosticEvent send (same `ComponentMessage` shape as the QsoFailed diagnostic in coordinator/qso.rs — copy it, target `"tx.placement"`).

- [ ] **Step 3:** `cargo test -p pancetta --lib` green; commit `"feat(coord): tx.placement degradation DiagnosticEvent"`

### Task 16: Auto-repark (opt-in)

**Files:**
- Modify: `pancetta-config/src/lib.rs`-adjacent (new `[tx_placement]` section — follow the `[hound]` section's file pattern: `pancetta-config/src/hound.rs` → create `tx_placement.rs`), `pancetta/src/coordinator/autonomous.rs` (act on it)
- Test: config default test + coordinator pure-fn test

**Interfaces:**
- Consumes: Task 15's threading of `tx_offset_hold_hz` + `active_tx_qsos: Arc<RwLock<HashSet<String>>>` (already in the autonomous task — used at :486).
- Produces: `TxPlacementConfig { pub auto_repark: bool /* default false */, pub repark_min_score_gain: f64 /* default 20.0 */ }`; repark decision fn.

- [ ] **Step 1: Failing pure-fn test:**

```rust
/// Repark ONLY when: enabled ∧ no active QSOs ∧ currently parked ∧ parked
/// bin busy-both (code 0) ∧ best slice beats the parked slice's CURRENT
/// score by ≥ min_gain. Returns the new offset to park at.
fn should_repark(
    enabled: bool, active_qsos: usize, parked_hz: u64,
    parked_coverage: Option<u8>, parked_score: Option<f64>,
    best: Option<&pancetta_qso::frequency::FrequencyCandidate>, min_gain: f64,
) -> Option<u64> { /* Task implements */ }

#[test]
fn repark_gates() {
    let best = pancetta_qso::frequency::FrequencyCandidate {
        offset_hz: 920.0, score: 95.0, clear_both_slots: true,
        clear_first: true, clear_second: true, noise_floor: 0.0,
    };
    // disabled → never
    assert_eq!(should_repark(false, 0, 1500, Some(0), Some(10.0), Some(&best), 20.0), None);
    // active QSO → never (LIVE STREAM SAFETY)
    assert_eq!(should_repark(true, 1, 1500, Some(0), Some(10.0), Some(&best), 20.0), None);
    // parked slice still usable (code 2) → hold (hysteresis)
    assert_eq!(should_repark(true, 0, 1500, Some(2), Some(60.0), Some(&best), 20.0), None);
    // busy-both + big gain → repark
    assert_eq!(should_repark(true, 0, 1500, Some(0), Some(10.0), Some(&best), 20.0), Some(920));
    // busy-both but marginal gain → hold
    assert_eq!(should_repark(true, 0, 1500, Some(0), Some(80.0), Some(&best), 20.0), None);
}
```

(`parked_score`: find the candidate nearest the parked hz in `snap.slices`, else re-derive — if absent from top-N, treat as `Some(0.0)`; document that.)

- [ ] **Step 2: Implement** config section (default OFF; validation none needed — booleans + positive float check mirroring hound.rs), the pure fn, and the loop wiring: on repark, `tx_offset_hold_hz.store(new_hz, Ordering::Relaxed)` + a `StatusUpdate` + a `DiagnosticEvent { target: "tx.placement", level: Info, text: "auto-reparked {old} → {new}" }`. Config docs line in the section's rustdoc: auto-repark adjusts the IDLE parked offset only; never a live stream (gated on `active_tx_qsos.is_empty()`).

- [ ] **Step 3: Phase gate:** fmt/clippy/full workspace test. Commit `"feat(coord): opt-in auto-repark with hysteresis (idle offset only, never mid-QSO)"`. **Phase 3 / PR 3 complete.**

---

## Phase 4 — Station card + Station Info demotion (PR 4, branch `feat/tui-station-card`)

### Task 17: Station card renderer

**Files:**
- Create: `pancetta-tui/src/ui/station_card.rs`
- Modify: `ui/mod.rs` (render in Station Info's slot in `layout_operate`; in `layout_hunt` below DX Hunter if ≥4 rows spare)
- Test: TestBackend test

**Interfaces:**
- Consumes: `app.focused_callsign()`, `app.decoded_messages` (newest decode from the focus: message text, snr, grid, distance, `slot_parity`, `needed`/`atno`/`worked_before` flags), `app.dx_stations` (entity_name if present), `app.active_qsos` (engaged → state string), `app.is_engaged`.
- Produces: `pub fn render_station_card(f, area, app) -> Result<()>` — 3 content lines: (1) `"{call} — {entity|'---'} {ATNO★|needed|worked|new}"`, (2) `"{grid} {dist}km · {last-msg-text} ({snr:+}) {age}"`, (3) engaged → `"in QSO: {state}"`, else `"{E|O}-window · Space=call"` (window = `slot_parity` of newest decode: `Even → "E"`). No focus → `"(no station focused — Tab to a list, ↑/↓ to pick)"`.

- [ ] **Step 1: Failing test:** app with one decode fixture (`K1AAA`, atno=true) + focus pinned → buffer contains `"K1AAA"` and `"ATNO"`; without focus → contains `"no station focused"`.

- [ ] **Step 2: Implement** (reuse `format_time_ago`, `format_distance` from `ui/mod.rs` helpers; entity via `app.dx_stations.get(call).and_then(|d| d.entity_name.clone())`).

  *Documented divergence from spec §4:* the spec sources "what they're doing right now" from the coordinator's `DxActivityMap`. That map reaches the TUI only for ACTIVE QSOs (`ActiveQsoSnapshotItem.dx_last_activity`). v1 derives activity from the newest `App.decoded_messages` entry for the focus — equivalent information for any on-band station the operator can focus. Threading full `DxActivityMap` summaries to the TUI for non-engaged stations is an additive follow-up if the last-decode view proves too thin.

- [ ] **Step 3:** tests green; commit `"feat(tui): station card renders the global focus"`

### Task 18: Demote Station Info

**Files:**
- Modify: `ui/mod.rs` (layouts: card replaces `render_station_info`; S-meter + audio level + device join `render_status_bar`), `app.rs` (`ActivePanel` — REMOVE `StationInfo`), `tui_runner.rs` (`1-5` jump keys renumber, help text), `ui/station_info.rs` (keep file; no longer routed)
- Test: update every test referencing `ActivePanel::StationInfo` (grep — includes tab-order tests)

**Interfaces:**
- Consumes: `render_status_bar` (ui/mod.rs:468) — add spans: `S:{db_over_s9:+}` (from `app.signal_strength_db`, `"---"` when stale per `signal_strength_at`), device short name.
- Produces: `ActivePanel` = `{BandActivity, TxPlacement, QsoStatus, Callers, DxHunter}`; jump keys `1`=Band, `2`=QSO, `3`=Callers, `4`=DX, `5`=TxPlacement; `focused_callsign`'s StationInfo arm deleted.

- [ ] **Step 1:** Delete the variant; chase every compile error (exhaustive matches in next/prev, focused_callsign, render highlight slices, jump keys, tests). Update the help overlay `1/2/3/4/5` line to `"Jump: Band/QSO/Callers/DX/Placement"`.
- [ ] **Step 2:** `cargo test -p pancetta-tui --lib` — fix every broken test's intent (tab-order tests get the new 5-ring).
- [ ] **Step 3: Phase gate** (fmt/clippy/workspace). Commit `"feat(tui): demote Station Info — card takes its slot, live bits join the status bar"`. **Phase 4 / PR 4 complete.**

---

## Phase 5 — Mouse + quick wins (PR 5, branch `feat/tui-mouse-quickwins`)

### Task 19: Mouse click-to-focus and click-to-park

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs` (pure `hit_test` fn mirroring the layout math), `app.rs::handle_mouse_event` (:955 — currently wheel-only)
- Test: hit_test unit tests (pure, no terminal)

**Interfaces:**
- Consumes: the per-view constraint math from Tasks 5-6 — **extract each view's rect computation into a pure `fn view_rects(view: ActiveView, zoomed: bool, area: Rect) -> Vec<(ActivePanel, Rect)>`** used by BOTH the layout fns and hit_test (single source of truth; this is the refactor that makes mouse reliable).
- Produces: `pub fn hit_test(view, zoomed, area, x, y) -> Option<(ActivePanel, u16 /*row within panel*/)>`; click behavior: set `active_panel`, move+pin that panel's cursor to the clicked row (row − header − border offsets per panel: table panels have 1 border + 1 header row); click on the TxPlacement strip row → park at the clicked column's bin freq (send SetTxOffset, same path as Task 12) — **collision-aware (spec §2): if the clicked bin's openness code is 0 (busy both windows), refuse with `status_message = "⚠ {hz} Hz busy in both windows — not parking"` instead of parking**; codes 1/2 park but the status names the busy window.

- [ ] **Step 1: Failing tests:** `view_rects(Operate, false, Rect::new(0,0,120,40))` returns 5 non-overlapping rects covering the content region; `hit_test` at a point inside Band Activity row 3 returns `(BandActivity, 3)`; a point on the status bar returns `None`.
- [ ] **Step 2: Implement** `view_rects` (move the `Layout::default()...split` chains), rewire layout fns through it, write `hit_test`, extend `handle_mouse_event` with `MouseEventKind::Down(MouseButton::Left)`.
- [ ] **Step 3:** tests + full suite; commit `"feat(tui): mouse click-to-focus + click-to-park (shared pure rect map)"`

### Task 20: Quick wins batch

**Files:**
- Modify: `ui/band_activity.rs` (columns), `ui/mod.rs` (status bar border row; chip styles; session counters), `tui_runner.rs` (`x` confirm; counters state), `app.rs`
- Test: per-item unit tests

Items (each its own commit, in order):

- [ ] **20a — Drop Band Activity's `Freq` and `Mode` columns** (both per-row constant: dial MHz and station-wide mode). Delete from header/widths/row; give `Msg` the space (`Min(20)`) and raise the truncation to 60 chars. Update any table-shape tests. Commit `"refactor(tui): drop per-row-constant Band Activity columns (dial freq, mode)"`.
- [ ] **20b — Adaptive column hiding:** in `render_band_activity`, when `area.width < 70` also drop `Dist`+`Grid`; `< 55` drop `DT`+`DF` (build header/widths/rows from one `enum`-driven column list so the three shapes can't drift). Test at TestBackend widths 50/65/120. Commit.
- [ ] **20c — Delete the decorative border row:** status bar `Length(3)` → `Length(2)` (ui/mod.rs:83 + the 3-way split at :613-620 → 2-way). Commit.
- [ ] **20d — Session counters:** count TUI-side from the diagnostic stream that already flows (no bus changes): in the `TuiMessage::DiagnosticEvent` handler, when `target == "qso"` && `level == Info` && `text.starts_with("QSO with")` (the exact completion text emitted by coordinator/qso.rs since PR #84), increment a new `app.session_completed: u32`. Render `"QSOs: {n}"` in the title bar before the clock (only when n > 0). Test: push two matching diagnostic events + one Warn → counter is 2. Commit `"feat(tui): session QSO counter in the title bar"`.
- [ ] **20e — Chip color dedup:** FREQ:HOLD / SPLIT / TX-offset / mode chips (ui/mod.rs:228-291) all become `fg(accent) + BOLD`, NO background; colored backgrounds remain ONLY for TX-policy, FOX (magenta), alarms (red), FCC-pause (yellow), TX (red). Commit `"style(tui): reserve chip background color for state that demands attention"`.
- [ ] **20f — Confirm on `x`:** first press sets `app.clear_armed_at = Some(Instant::now())` + status `"press x again within 3s to clear decodes"`; second press within 3 s clears. Test both paths. Commit.

- [ ] **Final phase gate:** fmt + clippy all touched crates + `cargo test --workspace --features transmit --exclude pancetta-research`. Update `CLAUDE.md` (TUI bullet: views/instrument/focus summary + key changes: `v V z`, renumbered jumps, removed StationInfo panel) and `pancetta-tui` module docs. Commit `"docs: TUI redesign shipped — CLAUDE.md + module docs"`. **Phase 5 / PR 5 complete.**

---

## Post-plan verification checklist (controller, after all 5 PRs)

- [ ] All 4 views render at 80×20 (minimum-size guard still triggers below) and 200×60.
- [ ] Operate view with zero placement data (autonomous task cold) shows the instrument's empty state, not a panic — `app.placement == None` renders `"waiting for first window…"`.
- [ ] SSH/16-color: openness strip glyphs legible (density glyphs already distinct); spot-check via `TERM=xterm ssh localhost`.
- [ ] On-air sanity (operator-gated): park via Enter → next manual call TXes on the parked offset (title chip agrees); auto-repark stays OFF by default.
