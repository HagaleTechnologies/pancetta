# DXCC Entity Display (#171) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Show the DXCC entity name of the station we're currently in QSO with (QSO Status panel)
and of our own station (title bar), reusing the entity-resolution pattern DX Hunter and the
Station card already established.

**Architecture:** Pure TUI-side, no coordinator/bus changes. QSO Status resolves the DX's entity
at render time via a new `resolve_entity` helper that checks `App.dx_stations` (cqdx-sourced, when
present) then falls back to the offline `crate::dxcc::entity_for_callsign` prefix table. The
title bar shows our own entity, computed once at `App::new` and cached on `StationInfo` (the home
callsign never changes at runtime).

**Tech Stack:** Rust workspace, `pancetta-tui` crate only.

## Global Constraints

- `cargo fmt` must be run for real (not `--check` alone) before every commit.
- Additive only: no existing field, function signature, or bus message changes.
- Entity omitted (not `(---)`) when unresolvable — the QSO Status panel has less room than DX
  Hunter's dedicated column, so an absent entity should stay invisible, not add noise.
- Multi-QSO table view (`render_multi_qso_table`) is NOT touched — entity display is single-QSO-
  detail-view only, matching the #165 QSO-history-line precedent.

---

## File Structure

- Modify `pancetta-tui/src/app.rs`: `StationInfo` gains `entity_name: Option<String>`, populated
  in `App::new`.
- Modify `pancetta-tui/src/ui/qso_status.rs`: new `resolve_entity` helper + wiring into
  `render_qso_info`.
- Modify `pancetta-tui/src/ui/mod.rs`: `render_title_bar` gains an entity span.

---

### Task 1: `StationInfo.entity_name`

**Files:**
- Modify: `pancetta-tui/src/app.rs:361-369` (`StationInfo` struct)
- Modify: `pancetta-tui/src/app.rs:1116-1128` (`App::new`)
- Test: `pancetta-tui/src/app.rs` (existing `#[cfg(test)] mod tests` block, near the bottom of the
  file)

**Interfaces:**
- Produces: `StationInfo.entity_name: Option<String>` — read by Task 3 (title bar).

- [ ] **Step 1: Write the failing test**

Add to the test module in `pancetta-tui/src/app.rs` (alongside the other `#[tokio::test]` fns):

```rust
#[tokio::test]
async fn station_info_resolves_own_entity_from_call_sign() {
    let mut config = crate::config::Config::default();
    config.station.call_sign = "K5ARH".to_string();
    let app = App::new(config, None).await.unwrap();
    assert_eq!(
        app.station_info.entity_name.as_deref(),
        Some("United States")
    );
}

#[tokio::test]
async fn station_info_entity_none_for_unresolvable_call_sign() {
    // Config::default()'s call_sign ("N0CALL") matches no real DXCC prefix.
    let app = App::new(crate::config::Config::default(), None).await.unwrap();
    assert_eq!(app.station_info.entity_name, None);
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-tui --lib station_info_resolves_own_entity_from_call_sign station_info_entity_none_for_unresolvable_call_sign`
Expected: FAIL — `entity_name` is not a field on `StationInfo` (compile error).

- [ ] **Step 3: Add the field and populate it**

In `pancetta-tui/src/app.rs`, change the `StationInfo` struct (lines 361-369):

```rust
#[derive(Debug, Clone)]
pub struct StationInfo {
    pub call_sign: String,
    pub grid_square: String,
    pub power: u32,
    pub antenna: String,
    pub rig: String,
    pub operating_frequency: f64,
    pub mode: String,
    /// Our own station's DXCC entity name (e.g. "United States"), resolved
    /// once from `call_sign` at construction — the home callsign is fixed
    /// for the process lifetime, unlike a QSO partner's. `None` if the
    /// callsign doesn't match any known prefix.
    pub entity_name: Option<String>,
}
```

In `App::new` (lines 1116-1128), add the field to the `station_info` construction:

```rust
    pub async fn new(config: Config, audio_device: Option<String>) -> Result<Self> {
        let station_info = StationInfo {
            call_sign: config.station.call_sign.clone(),
            grid_square: config.station.grid_square.clone(),
            power: config.station.power,
            antenna: config.station.antenna.clone(),
            rig: config.station.rig.clone(),
            operating_frequency: config.station.default_frequency,
            // Station-wide active operating mode (FT8/FT4/FT2) from [rig].mode
            // (carried on StationConfig); backs the title-bar mode chip
            // (rendered only when != FT8).
            mode: config.station.mode.clone(),
            entity_name: crate::dxcc::entity_for_callsign(&config.station.call_sign)
                .map(str::to_string),
        };
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-tui --lib station_info_resolves_own_entity_from_call_sign station_info_entity_none_for_unresolvable_call_sign`
Expected: PASS (2 tests)

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add pancetta-tui/src/app.rs
git commit -m "feat(tui): resolve own-station DXCC entity on StationInfo (#171)"
```

---

### Task 2: QSO Status panel shows the DX's entity

**Files:**
- Modify: `pancetta-tui/src/ui/qso_status.rs:192-305` (`render_qso_info`)
- Test: `pancetta-tui/src/ui/qso_status.rs` (existing `#[cfg(test)] mod tests` block, bottom of
  file)

**Interfaces:**
- Consumes: `App.dx_stations: HashMap<String, DxStation>` (existing), `DxStation.entity_name:
  Option<String>` (existing), `crate::dxcc::entity_for_callsign(call: &str) -> Option<&'static
  str>` (existing, `pancetta-tui/src/dxcc.rs:28`).
- Produces: `resolve_entity(app: &App, call_sign: &str) -> Option<String>` — a private helper in
  `qso_status.rs`, usable by any future addition to this file.

- [ ] **Step 1: Write the failing tests**

Add to the test module at the bottom of `pancetta-tui/src/ui/qso_status.rs`:

```rust
#[tokio::test]
async fn resolve_entity_prefers_cqdx_over_offline_table() {
    use crate::app::DxStation;
    let mut app = crate::app::App::new(crate::config::Config::default(), None)
        .await
        .unwrap();
    app.dx_stations.insert(
        "JA1ABC".to_string(),
        DxStation {
            entity_name: Some("Nippon (cqdx)".to_string()),
            ..test_dx_station("JA1ABC")
        },
    );
    assert_eq!(
        resolve_entity(&app, "JA1ABC").as_deref(),
        Some("Nippon (cqdx)")
    );
}

#[tokio::test]
async fn resolve_entity_falls_back_to_offline_table() {
    use crate::app::DxStation;
    let mut app = crate::app::App::new(crate::config::Config::default(), None)
        .await
        .unwrap();
    // In dx_stations (e.g. from a local decode) but cqdx never set entity_name.
    app.dx_stations
        .insert("JA1ABC".to_string(), test_dx_station("JA1ABC"));
    assert_eq!(resolve_entity(&app, "JA1ABC").as_deref(), Some("Japan"));
}

#[tokio::test]
async fn resolve_entity_falls_back_to_offline_table_when_never_seen() {
    let app = crate::app::App::new(crate::config::Config::default(), None)
        .await
        .unwrap();
    // Not in dx_stations at all (e.g. QSO partner never appeared in DX Hunter).
    assert_eq!(resolve_entity(&app, "DL1ABC").as_deref(), Some("Fed. Rep. of Germany"));
}

#[tokio::test]
async fn resolve_entity_none_when_unresolvable() {
    let app = crate::app::App::new(crate::config::Config::default(), None)
        .await
        .unwrap();
    assert_eq!(resolve_entity(&app, "QZ9ZZ"), None);
}
```

This test module needs a `test_dx_station` helper — add it alongside the other test helpers in
this same `mod tests` block:

```rust
fn test_dx_station(call: &str) -> crate::app::DxStation {
    crate::app::DxStation {
        call_sign: call.to_string(),
        grid_square: None,
        frequency: 14_074_000.0,
        mode: "FT8".to_string(),
        last_seen: chrono::Utc::now(),
        snr: -10,
        distance: None,
        bearing: None,
        worked_before: false,
        needed: false,
        atno: false,
        band_needed: false,
        priority_score: 0,
        source: crate::app::SpotSource::Local,
        entity_name: None,
        rarity_tier: None,
        reporter_count: None,
        is_notable: false,
        notable_type: None,
        confidence: None,
        best_snr_network: None,
        last_seen_network: None,
        audio_offset_hz: None,
    }
}
```

> **Note for the implementer:** `DxStation` may have additional fields beyond what's listed above
> (check the current struct definition at `pancetta-tui/src/app.rs:391` before writing this
> helper — the compiler will tell you exactly which fields are missing if this list is stale).

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-tui --lib resolve_entity`
Expected: FAIL — `resolve_entity` is not defined (compile error).

- [ ] **Step 3: Implement `resolve_entity` and wire it into `render_qso_info`**

Add near the top of `pancetta-tui/src/ui/qso_status.rs` (module-level, above `render_qso_status`):

```rust
/// Resolve a DX station's DXCC entity name for display: prefer cqdx's
/// authoritative `DxStation.entity_name` (already known for anyone who's
/// appeared in DX Hunter as a network/Both spot) and fall back to the
/// offline prefix table for locally-decoded-only stations, matching the
/// pattern `dx_hunter.rs::create_dx_row` and `station_card.rs::render_line1`
/// already use. `None` when neither resolves.
fn resolve_entity(app: &App, call_sign: &str) -> Option<String> {
    app.dx_stations
        .get(call_sign)
        .and_then(|d| d.entity_name.clone())
        .or_else(|| crate::dxcc::entity_for_callsign(call_sign).map(str::to_string))
}
```

In `render_qso_info` (`qso_status.rs:192-305`), after the existing `call_text` span is pushed
(around line 247-252), append an entity span when resolvable:

```rust
    status_line.push(Span::styled(
        call_text,
        Style::default()
            .fg(app.theme.accent_color())
            .add_modifier(Modifier::BOLD),
    ));
    // #171: DXCC entity of the station we're in QSO with, when resolvable.
    // Omitted entirely (not "(---)") when unresolved — this panel has less
    // room than DX Hunter's dedicated column.
    if let Some(call) = qso.call_sign.as_deref() {
        if let Some(entity) = resolve_entity(app, call) {
            status_line.push(Span::styled(
                format!(" ({entity})"),
                Style::default().fg(app.theme.muted_color()),
            ));
        }
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-tui --lib resolve_entity`
Expected: PASS (4 tests)

- [ ] **Step 5: Run the full qso_status test module to check for regressions**

Run: `cargo test -p pancetta-tui --lib qso_status`
Expected: PASS (all existing + 4 new tests)

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add pancetta-tui/src/ui/qso_status.rs
git commit -m "feat(tui): show DX's DXCC entity in QSO Status panel (#171)"
```

---

### Task 3: Title bar shows our own station's entity

**Files:**
- Modify: `pancetta-tui/src/ui/mod.rs:598-642` (`render_title_bar`)
- Test: `pancetta-tui/src/ui/mod.rs` (`mod view_render_tests`, bottom of file — uses `TestBackend`
  + `buffer_contains`, see existing tests near line 1581)

**Interfaces:**
- Consumes: `StationInfo.entity_name: Option<String>` (Task 1).

- [ ] **Step 1: Write the failing test**

Add to `mod view_render_tests` in `pancetta-tui/src/ui/mod.rs`:

```rust
#[tokio::test]
async fn title_bar_shows_own_station_entity_when_resolvable() {
    let mut config = crate::config::Config::default();
    config.station.call_sign = "K5ARH".to_string();
    let mut app = crate::app::App::new(config, None).await.unwrap();
    app.active_view = crate::view::ActiveView::Operate;
    let backend = TestBackend::new(120, 40);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &app).unwrap()).unwrap();
    let buf = term.backend().buffer().clone();
    assert!(
        buffer_contains(&buf, "(United States)"),
        "title bar missing own-station entity"
    );
}

#[tokio::test]
async fn title_bar_omits_entity_when_unresolvable() {
    // Config::default()'s call_sign ("N0CALL") resolves to no entity.
    let mut app = crate::app::App::new(crate::config::Config::default(), None)
        .await
        .unwrap();
    app.active_view = crate::view::ActiveView::Operate;
    let backend = TestBackend::new(120, 40);
    let mut term = Terminal::new(backend).unwrap();
    term.draw(|f| draw(f, &app).unwrap()).unwrap();
    let buf = term.backend().buffer().clone();
    assert!(!buffer_contains(&buf, "("), "unexpected parenthetical with no entity");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p pancetta-tui --lib title_bar_shows_own_station_entity_when_resolvable title_bar_omits_entity_when_unresolvable`
Expected: `title_bar_shows_own_station_entity_when_resolvable` FAILs (entity not yet rendered);
`title_bar_omits_entity_when_unresolvable` may pass vacuously already — that's fine, it still
guards the "omit when None" behavior going forward.

- [ ] **Step 3: Render the entity in the title bar**

In `pancetta-tui/src/ui/mod.rs`, in `render_title_bar` (lines 598-642), insert an entity span
right after the existing grid-square span (after line 619, before the `Span::raw(" | ")` at line
620):

```rust
        Span::raw(" | "),
        Span::styled(
            &app.station_info.grid_square,
            Style::default().fg(app.theme.foreground_color()),
        ),
    ];
    if let Some(entity) = app.station_info.entity_name.as_deref() {
        left_spans.push(Span::styled(
            format!(" ({entity})"),
            Style::default().fg(app.theme.muted_color()),
        ));
    }
    left_spans.extend([
        Span::raw(" | "),
        Span::styled(
            format!("{:.3} MHz", app.station_info.operating_frequency),
            Style::default().fg(app.theme.warning_color()),
        ),
        Span::raw(" "),
        Span::styled(
            app.config
                .get_current_band(app.station_info.operating_frequency)
                .map(|b| b.name.as_str())
                .unwrap_or(""),
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(" | "),
        Span::styled(
            &app.station_info.mode,
            Style::default()
                .fg(app.theme.accent_color())
                .add_modifier(Modifier::BOLD),
        ),
    ]);
```

> **Note for the implementer:** this restructures the `vec![...]` literal at lines 601-642 into a
> `let mut left_spans = vec![...]` (ending at the grid-square span) followed by the conditional
> push and an `.extend([...])` for the remaining spans. Read the current full literal first (lines
> 598-642) and adapt exactly — do not leave the original single `vec![...]` and also add a second
> one; there is only ever one `left_spans` binding.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p pancetta-tui --lib title_bar_shows_own_station_entity_when_resolvable title_bar_omits_entity_when_unresolvable`
Expected: PASS (2 tests)

- [ ] **Step 5: Run the full `view_render_tests` module to check for regressions**

Run: `cargo test -p pancetta-tui --lib view_render_tests`
Expected: PASS (all existing + 2 new tests) — pay particular attention to any test asserting exact
title-bar column positions or widths, since this changes its rendered length when an entity is
present.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add pancetta-tui/src/ui/mod.rs
git commit -m "feat(tui): show own station's DXCC entity in title bar (#171)"
```

---

### Task 4: Full workspace verification + PR

**Files:** none (verification only)

- [ ] **Step 1: Run the full pancetta-tui test suite**

Run: `cargo test -p pancetta-tui --lib`
Expected: PASS, 0 failures.

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -p pancetta-tui --all-targets -- -D warnings`
Expected: clean, no warnings.

- [ ] **Step 3: Run fmt check**

Run: `cargo fmt --check`
Expected: clean. If not, run plain `cargo fmt` and re-check (a non-empty `--check` diff means
unformatted code — fix it for real, don't treat the check itself as the fix).

- [ ] **Step 4: Full workspace build sanity check**

Run: `cargo build --workspace --exclude pancetta-research`
Expected: clean build, no errors (confirms nothing outside `pancetta-tui` broke, even though this
plan only touches that crate).

- [ ] **Step 5: Open the PR**

```bash
git push -u origin <branch-name>
gh pr create --title "feat(tui): DXCC entity in QSO Status + title bar (#171)" --body "$(cat <<'EOF'
## Summary
- QSO Status panel now shows the DXCC entity of the station we're in QSO with, next to the
  existing Call line — preferring cqdx's `entity_name` when known, falling back to the offline
  prefix table, same pattern DX Hunter and the Station card already use.
- Title bar now shows our own station's DXCC entity next to the grid square, resolved once at
  startup from the configured home callsign.
- Both omit the entity entirely (not "(---)") when unresolvable.

## Test plan
- [x] `cargo test -p pancetta-tui --lib` — all passing
- [x] `cargo fmt --check` clean
- [x] `cargo clippy -p pancetta-tui --all-targets -- -D warnings` clean

Closes #171
EOF
)"
```
