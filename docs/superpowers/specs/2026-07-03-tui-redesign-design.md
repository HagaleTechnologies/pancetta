# TUI redesign — activity views, TX-placement instrument, global station focus

**Date:** 2026-07-03
**Status:** Approved by operator (brainstorm session w/ visual mockups)
**Scope:** `pancetta-tui` + additive coordinator relay wiring. No TX-behavior changes.

## Problem

The TUI's fundamentals are right — a dense, single-screen, keyboard-first dashboard
matches FT8's 15-second glance→decide→keypress rhythm and the SSH/headless deployment.
Two structural weaknesses hold it back, plus one wrong-focus display:

1. **Rigidity.** One fixed percentage layout (`ui/mod.rs::draw`) serves every activity
   and every terminal size. Hunting DX, running a pileup (Fox), and band-monitoring
   need very different screen allocations; today everyone gets the same 60/40 grid,
   and no panel can be enlarged.
2. **Moving-target selection.** Every panel except QSO Status keeps a **positional**
   cursor (`band_activity_scroll`, `callers_scroll`, DX-Hunter `TableState`) into a
   list that reorders every 15 s as decodes land. The selection silently retargets
   between glance and keypress. QSO Status already solved this correctly
   (`qso_pinned_id` — cursor pinned to identity, not index); the fix never spread.
   The known DX-Hunter scroll bug is this class.
3. **The waterfall shows the wrong thing.** The operator's actual waterfall use case
   is TX placement: *find the most open slice across both transmit windows and park
   there; for multi-TX, several open slices*. Today's waterfall renders energy
   (where you *can't* go), never ranks openness, forces the operator to intersect
   the Even/Odd occupancy strip by eye — and the intelligence that answers the real
   question already exists unshown: `SmartFrequencyAllocator::score_candidate`
   (`pancetta-qso/src/frequency.rs`) scores every 25 Hz candidate with
   **clear-in-both-slots as its strongest criterion (+30)**, plus noise floor,
   neighbor guard, decode history, and own-stream separation. The autonomous path
   uses it every allocation; the `t` key uses it once; the display never shows it.

Secondary issues folded in: Station Info spends 20% of the right column on static
config; Band Activity carries two per-row-constant columns (dial `Freq`, `Mode`);
one status-bar row is a decorative border; the waterfall draws only ONE TX marker
even during multi-TX; cyan means four different things across title-bar chips;
`x` clears all decodes without confirm; no session counters anywhere.

## Design

### 1. Activity views

Four layout presets over the same `App` state. A view changes **only** the layout
function — which panels render and their constraints — never the data model.

| View | Layout emphasis |
|---|---|
| **Operate** (default) | Today's layout, refined (quick wins applied). The safe home. |
| **Hunt** | DX Hunter dominant (full entity names + need-status column), Band Activity filtered to CQs, TX-placement strip, QSO Status compact until a QSO starts. |
| **Run** | Callers + multi-QSO table own the screen (pileup / Fox serving). Energy waterfall and DX Hunter fold away; the 5-row TX-placement strip stays — its per-stream TX markers matter most here. |
| **Monitor** | Full-height energy waterfall + full-width Band Activity with every column. RX-watching / propagation. |

- **`v`** cycles views (`V` reverse). Both keys are unbound today.
- Current view name renders in the title bar (`HUNT` / `RUN` / `MON`; Operate shows
  nothing — today's title bar unchanged in the default view).
- Last view persists across restarts in a small TUI state file
  (`~/.pancetta/tui_state.json` — runtime state, not operator config; do NOT put it
  in the TOML).
- Mode changes **suggest, never auto-switch**: engaging Fox emits a status hint
  ("FOX on — press v for Run view"). Spatial memory is sacred at a 15 s cadence.
- Implementation shape: `enum ActiveView { Operate, Hunt, Run, Monitor }` on `App`;
  `draw()` dispatches to per-view layout functions that share the existing panel
  renderers. A pure `fn view_layout(view, area) -> ViewChunks` mapping is unit-testable
  without a terminal.

### 2. TX-placement instrument (replaces the waterfall in Operate/Hunt/Run)

A ~5-row instrument focused on **vacancy, not energy**:

```
┌ TX Placement ──────────────────────────────────────────────┐
│ OPEN ████▒▒▄▄▄▒▒▒▒██████▒▒▀▀▒▒▒████▒▒▒▒▒██   █=open both  │
│      ▲310    ▲920    ▲1480      ▲2140        ▄=odd only    │
│ BEST ① 1480 E+O 98  ② 920 E+O 91  ③ 2140 E+O 84  ④ 310 O  │
│ parked: 1480 (E+O, holding 6 min) · Enter=park ① · z=full  │
└────────────────────────────────────────────────────────────┘
```

- **Openness strip:** per-bin tri-state — bright/full block = clear in BOTH windows,
  half-blocks = clear in one (glyph distinguishes which), dark = occupied. Vacancy is
  the figure; energy is the ground.
- **BEST row:** the allocator's live top slices, re-ranked every 15 s window.
  Multi-TX takes ①②③ directly — candidates are already separation-scored against
  each other (criterion 7, −50 inside `min_separation_hz`).
- **Park line:** current parked offset, window coverage, hold duration.
- **`z` on the strip** opens the full **top-10 panel**: freq, E/O/both, score, gap
  width, minutes-quiet.
- **Interactions:** `Enter` parks at the highlighted/best slice (flows through the
  EXISTING `tx_offset_hold_hz` + `TxFreqMode::Hold` machinery — same as the `o`
  modal and `t` key; no new TX plumbing). `←/→` moves a frequency cursor for manual
  placement; mouse click parks at the clicked bin (collision-aware refuse/warn).
- **Data source — single-scorer invariant:** the coordinator computes the ranking by
  running the same `SmartFrequencyAllocator` the autonomous path uses (real
  `SpectralSnapshot` + `DecodeHistory`), once per decode window, and pushes an
  additive `TuiMessage::TxPlacementUpdate { slices, .. }` (mirroring the
  `DiagnosticEvent` relay pattern from PR #84). The TUI never re-derives scores —
  the panel and autonomous picks can never disagree.
- The full energy waterfall remains in **Monitor view** and as the zoom target
  there. (Optional later polish, explicitly deferred: half-block ▀▄ rendering for
  2× vertical resolution in Monitor.)

### 3. Park semantics

- **Manual park (default):** operator parks; pancetta holds the offset. When the
  parked slice degrades (occupancy appears within the guard band in either window),
  the instrument flags it — "parked slice busy in E — ② 920 better" — and the
  operator decides. Never moves under them.
- **Auto-repark (opt-in):** config flag + runtime toggle. Re-parks onto the best
  slice only when the current one degrades past a hysteresis threshold (not for
  marginal gains), **never mid-QSO** — auto-repark adjusts the *idle parked offset
  only*; it never moves a live stream's TX frequency (the stuck-DX hop remains the
  only mid-QSO mover, unchanged). Every auto-repark is logged + surfaced as a
  DiagnosticEvent.

### 4. Global station focus + station card

- **One focus, many engagements.**
  - **Focus** (singular): the station under the operator's attention. Callsign-pinned
    (extend the proven `qso_pinned_id` pattern to Band Activity, DX Hunter, Callers —
    kills the moving-cursor bug class including the known DX-Hunter scroll bug).
    Shared across all panels: select VK9DX anywhere → highlighted everywhere,
    including a marker on the TX-placement strip. `Space`/`Enter` act on the focus
    from any panel (the existing context-resolved Space semantics unchanged).
    If the focused station ages out of a list, the cursor degrades gracefully to the
    nearest row — never silently retargets.
  - **Engagements** (plural): every active QSO stream. Secondary highlight in every
    list (green `●` gutter dot + underline) and a **labeled solid TX marker on the
    strip per stream** — fixing the current single-`tx_offset` gap in the Waterfall
    widget (thread `QsoManager::active_tx_offsets()` through the snapshot).
- **Station card:** a compact panel that appears where Station Info used to live,
  describing the focus: entity + need status (ATNO/needed/worked — already threaded
  into `DecodedMessageView`), grid/distance, what they're doing right now (existing
  `DxActivityMap` / `dx_activity_summary`), which window they transmit in, and the
  actions that currently apply (`Space=call at ① 1480`, `Shift+H=hound`). Focusing
  an engaged station shows its QSO ladder in the card.

### 5. Structural riders

- **`z` = zoom** the focused panel full-screen (toggle). Works in every view.
- **Station Info demoted:** static fields (power, rig name, sample rate, grid) move
  out of the main grid; live bits (S-meter, audio level, device status) fold into
  the title/status bars. Its grid slot goes to the station card.
- **Mouse:** click row = focus; click strip = park; wheel = scroll (exists). No
  drag interactions in v1.
- **Quick wins:** drop Band Activity's `Freq` + `Mode` columns (both per-row
  constant; reclaim ~12 chars for `Msg`); delete the decorative status-bar border
  row; session counters (QSOs today / last hour) in the title bar; de-duplicate
  chip colors (cyan currently = FREQ:HOLD, SPLIT, TX-offset, and mode chips — give
  info-chips one neutral style, reserve color for alarms); confirm on `x`;
  adaptive column-hiding on narrow terminals (drop Dist/Grid before crushing all).

### 6. Explicitly out of scope (deferred, not rejected)

- Command palette (`:` fuzzy commands) and the broader keymap-safety overhaul
  (`h`/`H`, `t`/`T`, `p` sharp edges) — worth doing, separate effort.
- Half-block waterfall rendering (Monitor polish).
- Replay/demo harness (operator chose direct-on-live-TUI iteration).
- Any change to TX scheduling, parity, or QSO-engine behavior. The instrument only
  changes how the operator *chooses* an offset; everything flows through existing
  hold/park machinery.

## Phasing — five independently-shippable PRs

1. **Selection foundation.** Callsign-pinned cursors in Band Activity / DX Hunter /
   Callers + the global-focus model (focus state on `App`, cross-panel highlight).
   Pure bug-fix value even if nothing else ships.
2. **View scaffold.** `ActiveView` enum, `v`/`V` cycling, per-view layout dispatch,
   state-file persistence, title-bar view name, `z` zoom. Operate = exactly today's
   layout; Monitor = trivial recomposition of existing panels. Hunt/Run land here
   as recompositions too (CQs-only filter for Hunt's Band Activity), using the
   existing waterfall in slim form as a placeholder until phase 3 swaps in the
   instrument.
3. **TX-placement instrument.** Coordinator-side per-window allocator run +
   `TxPlacementUpdate` relay; openness strip + BEST row + park line; top-10 zoom
   panel; park interactions; degradation warnings; multi-TX markers. Auto-repark
   (opt-in) can trail in its own PR if this one runs long.
4. **Station card + Station Info demotion.**
5. **Mouse + remaining quick wins** (several quick wins can ride earlier PRs where
   they touch the same files).

Each PR keeps the workspace green and the default experience (Operate view)
recognizable; nothing forces relearning until the operator presses `v`.

## Testing

- Pure-function unit tests: `view_layout()` constraint mapping per view;
  sticky-cursor invariant (list reorder preserves the selected callsign; age-out
  degrades to nearest row); slice-ranking display ordering (given a canned
  `TxPlacementUpdate`).
- Allocator: the ranking reuses existing `SmartFrequencyAllocator` tests; add one
  coordinator-side test that the per-window run feeds the relay message.
- `ratatui::backend::TestBackend` golden snapshots: one per view at 2 terminal
  sizes, plus the instrument strip with 0/1/3 active TX streams.
- Park-mode tests: manual park never moves without input; auto-repark respects
  hysteresis + never fires with an active QSO on the parked stream.
- Existing TUI tests (164) must stay green throughout — the Operate view is a
  regression oracle for phases 1–2.

## Risks

- **Spatial-memory churn:** mitigated by Operate-stays-default, suggest-don't-switch,
  and phasing (nothing moves until `v`).
- **Coordinator→TUI message volume:** `TxPlacementUpdate` is once per 15 s window —
  negligible next to the decode stream.
- **Allocator cost per window:** it already runs per autonomous allocation; one
  extra scored sweep (~96 candidates at 25 Hz step) per window is trivial.
- **Focus-follows-callsign edge cases:** compound callsigns must match via the
  existing `base_callsign`/`callsigns_match` equivalence (C18) so `EA8/G8BCG` and
  `G8BCG` are one focus target.
