<!-- Covers: TUI. Content moved from CLAUDE.md 2026-07-07.
     For current behavior trust the code and specs in `docs/superpowers/specs/`. -->

# TUI

## TUI redesign — 4 activity views + TX-placement instrument + global focus + mouse (2026-07-03, 5 PRs)

`pancetta-tui/src/{app.rs,view.rs,ui/{mod.rs,tx_placement.rs,station_card.rs},tui_runner.rs}`, `pancetta-qso/src/{frequency.rs,autonomous.rs}`, `pancetta/src/{message_bus.rs,coordinator/{autonomous.rs,tui_relay.rs}}`, `pancetta-config/src/tx_placement.rs`: a ground-up TUI redesign, subagent-driven-development, spec `docs/superpowers/specs/2026-07-03-tui-redesign-design.md`, plan `docs/superpowers/plans/2026-07-03-tui-redesign.md`. **4 activity views** (`view::ActiveView` = Operate/Hunt/Run/Monitor, `v`/`V` cycles, persisted to `~/.pancetta/tui_state.json`): Operate (default, unchanged layout until the instrument swap below), Hunt (DX Hunter full-width + a CQs-only-filtered Band Activity — the filter lives in `App::displayed_messages()` itself so cursor/renderer/Space all agree on what's visible), Run (Callers full-width for pileups), Monitor (the original big-waterfall view, explicitly untouched by everything else in this redesign). **TX-placement instrument** (`ui/tx_placement.rs`) replaces the waterfall in Operate/Hunt/Run: a vacancy-first 5-row panel (openness strip color-coded by per-slot [First/Second, i.e. even/odd] clear flags, live-QSO stream markers, a ranked BEST-frequency list, a park line, a frequency axis) computed via `AutonomousOperator::placement_snapshot()` in the coordinator's autonomous tick from the exact same `SmartFrequencyAllocator`/history/spectral-snapshot the real autonomous decision path uses -- the single-scorer invariant: the display never diverges from what autonomous would actually pick (a CQ-mode live-spot rarity boost initially missing from the display was found in review and fixed by extracting it into one shared function both paths call). Operator interactions: Left/Right steps the ranked list, Enter parks via the existing `TuiCommand::SetTxOffset` (same command the pre-existing `o` modal uses -- no new TX path), `z` zooms to a top-10 table, edge-triggered degradation warnings (transient TUI status line + persistent coordinator `DiagnosticEvent`) fire once when a parked frequency's coverage worsens. **Opt-in (default OFF) auto-repark** (`[tx_placement]` config) is the only feature in this whole redesign that autonomously writes to a live TX-affecting atomic (`tx_offset_hold_hz`) -- gated by a pure 5-case safety contract (never while any QSO is active -- verified zero `.await` between the live-QSO check and the write, plus an architectural backstop: the atomic is only ever consulted at new-manual-QSO-open time, latched once, never re-read for an in-flight transmission) and fails closed on a poisoned lock. **Global station focus** (`App::focused_callsign()`, Phase 1): one callsign-pinned focus shared across every panel (compound-aware via `pancetta_core::callsign`), with a two-tier highlight -- the focused station (bold+accent, any panel) and every engaged (live-QSO) station (green bullet+underline, every list, all the time -- the multi-TX visibility gap the old single-TX-marker waterfall couldn't show). Space acts on the global focus from any panel. **Station card** (`ui/station_card.rs`) replaces the Station Info panel entirely (`ActivePanel::StationInfo` removed) with a 3-line summary of the focused station; live S-meter/audio-level/device info moved to the status bar. **Mouse support** (Task 19): click any row to focus+pin it (mirrors Tab+arrows); click the TX-placement openness strip to park there (collision-aware -- refuses with a warning if the clicked bin is busy in both windows). A shared pure `view_rects`/`hit_test` pair is the single source of truth both the renderer and the mouse handler read, so a click always maps to what's actually on screen. **Key-binding changes:** `v`/`V` (view cycle), `z` (panel zoom, orthogonal to the active view), `1`-`5` renumbered (Band/QSO/Callers/DX/Placement -- Station Info's old slot 5 dropped), `x` (clear decodes) now requires a confirming second press within 3s.

## DX-activity panel (#41)

`pancetta/src/coordinator/qso.rs`, `pancetta-tui/src/ui/qso_status.rs`: a band-wide `DxActivityMap` (callsign → latest decoded-frame summary, 150s TTL + 256-entry cap) written by the decode loop via `record_dx_activity`; pure `dx_activity_summary` renders "calling CQ" / "→ W1XYZ R-12" / "→ us -09" (says "us" when directed at our station; compound-aware). `build_active_qso_snapshot` enriches each `ActiveQsoSnapshotItem.dx_last_activity` (compound-aware lookup), threaded through `ActiveQsoBanner` → `QsoStatus` → a "DX: …" line in the QSO-detail panel — so even before the DX answers, the operator sees whether they're working someone else, calling CQ, or coming back to us.

## Pending frequency-drift confirmation surfaced to the operator (PAN-142, 2026-09-12)

Same `ActiveQsoSnapshotItem` → `ActiveQsoBanner` → `QsoStatus` pipeline as the DX-activity panel above, one more pair of fields: `pending_freq_drift_hz`/`pending_freq_drift_since` mirror `QsoMetadata::pending_freq_drift` (the two-strike anti-spoof frequency-drift-confirm candidate, `maybe_confirm_frequency_drift_at` — see `docs/DECISIONS/qso-engine.md`), rendered as "Drift candidate: N Hz, confirming, Xs ago" in the QSO-detail panel next to the "DX: …" line. Before this, a QSO silently waiting on a second confirming sighting (about to self-heal in the next slot or two) looked identical to one genuinely stuck (auto-sequence re-sends the same message either way) — recurred across 3 live incidents (LU7LRP, K6L/#245, W6O) with the operator reaching for a manual override without knowing whether it was actually necessary. Cosmetic only; no TX-path/relevance-gate behavior changed.

## Tri-state global TX policy

`pancetta_core::TxPolicy` = `Full` | `RespondOnly` | `Disabled`, default `Full`: the operator's master TX switch, stored in the coordinator as `tx_policy: Arc<AtomicU8>` (orthogonal to `autonomous_enabled_runtime`). Operator cycles it with `g` in the TUI (Full → RespondOnly → Disabled → Full); Shift+Q emergency stop forces `Disabled`. Each state is echoed back as `MessageType::TxPolicyStatus` → `TuiMessage::TxPolicyUpdate` and rendered as a **bold, color-coded title-bar banner** (GREEN "TX: FULL", YELLOW "TX: RESPOND-ONLY", RED "TX: DISABLED — RX ONLY"). Gating map: **Disabled** = hard mute at the TX worker (`coordinator/tx.rs`, both `TransmitRequest` and `MultiTransmitRequest` arms) — never keys PTT / plays audio / modulates, consumes the request, reports a failed `TransmitComplete` + clears the TUI TX view. **RespondOnly** = suppress *initiations only*, gated at the sources: `StartCq` + `CallStation` refused in `tui_relay.rs` (with operator warning), repeating-CQ loop stopped, and autonomous initiation items (the `qso_id == None` `OperatorAction::Transmit`s, i.e. CQ-self + hunt/pounce) dropped in `coordinator/autonomous.rs` while QSO-in-progress items (`qso_id == Some`) and `RespondToCaller`/`QsoEvent::MessageToSend` flow through normally. **Full** = everything (legacy behavior). Autonomous initiation additionally requires the `autonomous_enabled_runtime` gate open. A **NOW-SENDING / QUEUED** TX strip (`pancetta-tui/src/ui/mod.rs::render_tx_strip`) shows the keyed message text + audio freq and items dequeued-but-not-yet-on-air, fed by `MessageType::TxQueueStatus { sending, queued }` from the TX worker (lightweight scope: reflects the request the worker is currently scheduling, not a deep channel-backlog scan).

## TX-policy events wired into the Diagnostics overlay (2026-07-12)

`pancetta/src/coordinator/tx.rs`: observability-diagnostics-plan.md Layer 1's "Emission" item (the highest-value existing `warn!`/`info!` sites should *also* emit a `DiagnosticEvent`, not just log to file) is done for the `"tx.policy"` category — all 10 sites (TxPolicy-Disabled hard-mute for single-TX/multi-TX/tune, stale-TX-at-defer-time drops, stale-TX-at-key-time drops, per-item and whole-bundle multi-TX stale drops, backlog-coalesce reporting, non-TX-message re-enqueue failure) now call a new generic `emit_diagnostic(bus, target, level, text, qso_id)` helper alongside their existing log line, so these already showed up in the log file but now also show in the TUI's Diagnostics overlay (Shift+D) live. The `qso.security` follow-up shipped 2026-07-31: `pancetta-qso` emits typed `QsoEvent::MessageRejected` events without changing state-machine return types or accept/reject decisions, and the coordinator translates them into retained yellow Warn rows with the rejected callsign. Routing-stage rejections in `is_message_relevant` are included because they can shadow the transition-level guards; explicit self-call refusals in `qso.rs` and `tui_relay.rs` use the same path while keeping their existing status messages. Same pass also fixed each site's own `target:` string for the co-located `tracing` macro call (`"tx.policy"` → `"pancetta::tx.policy"`) — the bare string fell outside the default `EnvFilter` directive (`pancetta={level},warn`) so `info!`-level lines were silently dropped from the log file too, a separate bug from the bus-wiring gap (found while fixing hb-062's FP filter, see `docs/DECISIONS/qso-engine.md`'s 2026-07-12 entry, which flags ~30 other sites across the coordinator + `pancetta-dx`/`pancetta-agent`/`pancetta-config` with the same tracing-target gap, not yet fixed). Tests: `coordinator::tx::tx_failure_diagnostic_tests::emit_diagnostic_carries_caller_supplied_target_and_level`.

With `qso.security` shipped, the scoped observability-diagnostics plan is complete: the structured Recent-QSOs ring/panel and optional per-QSO timeline persistence have also landed. Autonomous skip/pounce and keep-call-watchdog diagnostics remain possible follow-up emission sites, not blockers for the completed sender-verification visibility work.

## Station health panel — Layer 3 (2026-07-12)

`pancetta-tui/src/{app.rs,tui_runner.rs,ui/mod.rs}`, `pancetta/src/coordinator/{tx.rs,ft8.rs,tui_relay.rs,health.rs,mod.rs}`, `pancetta/src/main.rs`: closes `docs/observability-diagnostics-plan.md` Layer 3. Shift+S opens a consolidated "is the station healthy right now?" snapshot — audio/dsp/decoder liveness and decode timing (from the existing `PipelineHealth` tick), rig connection + TX-output-default (existing `App` fields), decode/watchdog panic counts (`DECODE_PANIC_COUNT`/`PANIC_COUNT` already existed and incremented correctly but had zero consumers outside their own module), and 4 new session counters: TX attempts/defers (new process-global `AtomicU64`s in `tx.rs`, matching the existing panic-counter pattern — no new locking/message-passing) and QSO-failed/TX-drop counts (TUI-side tallies from the existing `DiagnosticEvent` stream, sibling to the pre-existing `session_completed` counter — no new coordinator plumbing). Mirrors the Shift+D Diagnostics overlay's open/close pattern exactly, minus scroll state (a snapshot, not a scrollback).

**Surfaced a real cross-crate architecture bug along the way**: `pancetta`'s `Cargo.toml` splits it into a `pancetta_lib` LIB crate (all of `coordinator/`, including `tui_relay.rs`) and a `pancetta` BIN crate (`main.rs`, where `PANIC_COUNT` originally lived). A lib crate can never reach into its own bin crate — the original plan text's `crate::panic_count()` call from `tui_relay.rs` could not compile. Fixed by moving `PANIC_COUNT` + `record_panic()`/`panic_count()` into `coordinator/health.rs` (already in the lib crate) and re-exporting just those two functions at `pancetta_lib::coordinator::{record_panic, panic_count}` via the coordinator module's existing `pub use health::{...}` block (the same pattern already used for `classify_config_reload` et al.) — `mod health` itself stays private, so nothing else in `health.rs` becomes part of the lib crate's public surface. `main.rs`'s panic hook now calls `pancetta_lib::coordinator::record_panic()`.

Plan: `docs/superpowers/plans/2026-07-12-health-panel.md`.

## Keybindings single source of truth (2026-07-14, Onboarding Phase 2)

`pancetta-tui/src/keymap.rs` (new): the `?` help overlay (`tui_runner.rs::render_help_overlay`,
36 hand-maintained entries) and README's keybinding table (~40% wrong/incomplete, missing
`Shift+Q` EMERGENCY STOP among ~14 missing bindings) had drifted apart independently. Both now
read from one `KEYBINDINGS: &[KeyBinding]` table (39 entries — the original 36 plus `g`
cycle-TX-policy, `Shift+M` cycle-operating-mode, `e` cycle-decode-effort, which the overlay had
silently never gained). Each `KeyBinding` carries a `Category` (7 variants, doc-section grouping)
and an `essential: bool` (the 10 entries README's shrunk table shows). `render_markdown()`
generates `docs/KEYBINDINGS.md` (full reference, GENERATED header pointing at the regen command)
from the same table — drift-guarded by `keymap::tests::keybindings_doc_is_current`, same
philosophy as `pancetta-config`'s `merge_with` guard: a stale generated doc fails a test, not a
human review. `essentials_table_is_exactly_ten_rows` and `no_duplicate_keys` are companion
guards. Regen: `PANCETTA_REGEN_DOCS=1 cargo test -p pancetta-tui keybindings_doc_is_current`.
README's own keybinding table shrank to the 10 essentials + a link to the generated doc (or `?`
in the TUI) rather than trying to stay a second hand-maintained copy. Plan:
`docs/superpowers/plans/2026-07-13-onboarding-phase2-docs-truth.md` Task 7.

## RX input-device fallback badge + rigctld error surfacing (2026-07-14, Onboarding Phase 4)

Two "the RIG/AUDIO badge always has a reason" fixes from
`docs/superpowers/plans/2026-07-13-onboarding-phase4-five-minute-on-air.md` (doctor + wizard
rig step from the same plan are in `docs/DECISIONS/config-and-platform.md`'s 2026-07-14 entry).

**RX input-device fallback:** the TX-output side already surfaced a misconfigured/system-default
output device as a persistent TUI badge; the RX input side had the equivalent failure
(configured `[audio] input_device` not found, capture silently falls back — "the classic
decoding-the-built-in-mic trap") but only `warn!`-to-log. Mirrored the existing pattern end to
end across 8 files (`pancetta-audio/src/{stream,manager}.rs` record + expose the fallback;
`pancetta/src/coordinator/{mod,audio,tui_relay}.rs` latch an `Arc<AtomicBool>`, emit a one-shot
`MessageType::Error`, push a `TuiMessage` only on change; `pancetta-tui/src/{tui_runner,app}.rs`
+ `pancetta-tui/src/ui/station_info.rs` render a `⚠ RX→fallback device` badge). Additive only —
confirmed no TX-scheduler/armed-gate/parity/coalesce-window files touched, loopback QSO tests
unaffected. Known minor gap: `pancetta-tui/src/ui/mod.rs`'s separate compact status-line "dot"
(which shows TX-output status) wasn't extended to show the RX-input-fallback state — real but
small, out of this task's 8-file scope.

**rigctld failure surfacing:** rigctld spawn/model/connect failures previously only `warn!`-to-
file — invisible under the TUI's alternate screen, so a RIG ✗ badge had no visible reason. A new
`report_rig_error` helper (`pancetta/src/coordinator/hamlib.rs`) emits both an ephemeral
`MessageType::Error` and a retained `DiagnosticEvent` (`target: "rig.cat"`, Shift+D overlay) at
all four existing failure sites (suspicious `[rig.interface].port`, `rigctld` spawn failure,
unknown rig model, connect failure). The `rig_enabled` guard on the connect-failure site keeps a
disabled/mock rig silent — a missing guard there would have made every disabled-rig (the
default) station show a spurious rig error.

## Last-10-QSOs history panel (issue #165), landed 2026-07-19

The QSO Status panel's single-QSO-detail layout gained a compact history line
("✓K5ARH ✗JA1ABC ..."), most recent first, capped at 10 (`App.qso_history`). Wired via a new
`MessageType::QsoHistoryEntry` bus push (emitted from the coordinator's existing
`QsoEvent::QsoCompleted`/`QsoFailed` handlers, alongside the existing `ActiveQsosSnapshot` push)
relayed to a new `TuiMessage::QsoHistoryEntry`. "Successful" = `QsoCompleted`; any `QsoFailed`
reason renders the ✗ glyph (not color-only, for accessibility). The multi-QSO table view does not
get this line — only the single-QSO-detail layout.

## Shift+Q emergency stop is live in every modal (PAN-113, 2026-10-10)

`pancetta-tui/src/tui_runner.rs::handle_key_event`: the emergency-stop check now runs right after
the app write-lock, ahead of all 13 modal/overlay blocks (out-of-band ack, quit confirm, help,
audio picker, rig picker and its two bookmark sub-states, frequency entry, TX-offset entry,
composer, Diagnostics, Station Health, Recent QSOs). Each block ends in `_ => {}` +
`return Ok(true)`, so the old `Char('Q')` arm in the main match was dead whenever any of them was
open (P0 #2 in `docs/reviews/2026-09-05-product-review/lane1-tui.md`). One check ahead of the chain
was chosen over a `Q` arm per block (the pattern PAN-24 used for `k`) so a new modal can't forget
it. Any modifiers are accepted: PAN-112's Ctrl/Alt guard is for keys that start TX and does not
apply to the stop. The open dialog is left as it was. Each Esc closes one dialog layer, and the
Esc after the last layer clears the banner: two presses for the other dialogs, three from the rig
picker's bookmark list or bookmark-name prompt, whose first Esc returns to the picker form.
Accepted trade-off: in the composer, rig model and bookmark-name fields, Shift+Q (and
Caps Lock + q, which crossterm also delivers as `Char('Q')`) stops the station instead of typing
`Q`. The composer uppercases input and rig-model lookup ignores case, so lowercase `q` covers both.
Tests: `tui_runner::key_tests::shift_q_emergency_stops_through_every_overlay` and the four
`shift_q_*` / `modified_shift_q_*` tests beside it.

## Clock-skew alarm chip (PAN-114, 2026-10-10)

`pancetta-tui/src/ui/mod.rs` (title-bar alarm chain), `pancetta-tui/src/app.rs::PipelineHealth`:
the title bar shows `⚠ CLOCK {mag} {SLOW|FAST}` while the coordinator's clock-skew monitor reports
|offset| ≥ 0.3 s vs pool.ntp.org. `{mag}` is `{:.2}s` below 10 s and `{:.0}s` from 10 s up (a Pi
booted with no RTC can be a day off: `⚠ CLOCK 86400s SLOW`). SLOW means the local clock is behind
UTC (positive SNTP offset). It reuses the alarm slot and style (bold white on red) at the **lowest**
priority, after `AUDIO DEAD` and `DECODER STUB`, so it never competes with a second alarm chip for
width; only the highest-priority alarm shows.

Contract: `PipelineHealth.clock_skew_warning_s: Option<f64>` is `Some` only while the monitor is
warning, and `None` while the first probe is pending, when pool.ntp.org is unreachable, when the
clock is in spec, and right after a detected clock step. The threshold and all policy live in the
coordinator (`pancetta/src/clock_skew.rs`); the TUI only formats (`clock_skew_chip_text`,
`readiness::clock_skew_label` since PAN-89), because `pancetta-tui` cannot depend on `pancetta_lib`. The 2 s `tui_relay` health tick copies the value.

No key hint: a `— Shift+D` suffix truncated to `— S` at 120 columns. The Diagnostics overlay carries
the full `clock.skew` / `clock.step` text and the per-OS NTP fix. At 100 columns every alarm chip is
clipped to `⚠`; that pre-existing behaviour is unchanged.
Superseded by PAN-88 (below): the alarm became title-bar admit rank 4, shown whole or dropped, never
clipped. PAN-89 (below) then moved it out of the title bar into the readiness strip.

## Narrow-terminal layout: reserved clock, priority-fit chips, adaptive columns, scrollable help (PAN-88, 2026-10-12)

`pancetta-tui/src/ui/bar_fit.rs` (`BarSegment`, `fit_bar`, `bar_width`, `bar_spans`, `prefix_fit`),
`pancetta-tui/src/ui/mod.rs` (`title_rank`, `title_bar_segments`, `title_bar_left_line`,
`render_title_bar`, footer in `render_status_bar`), `ui/dx_hunter.rs` and `ui/callers.rs`
(`visible_columns`), `pancetta-tui/src/tui_runner.rs` (`help_overlay_geometry`, `help_body_lines`,
help key branch): at 80×24 and 100×30 the title bar lost the UTC clock, DX Hunter's headings read
`G S R R L P`, Callers' read `SN Fr A Fl`, the footer ended in `q:Qui`, and the `?` overlay silently
cut off every binding past row ~17, including `Shift+Q`.

Title bar: the clock is drawn in its own `Length(12)` rectangle at the right edge, so geometry, not
arithmetic, guarantees it (a 40-character callsign can't clip it). The other chips go through one
ordered step list (`title_rank`): each chip has an admit step (compact form if it has one) and,
with a compact form, an upgrade step; steps apply greedily by rank and a step that overflows is
reverted without blocking later ones. Callsign and TX policy are pinned. Admit order: live `TX`,
`⏸` pause prompt (compact `⏸ CQ PAUSED: press a key`), mode, frequency/band (compact
drops `MHz`), `FOX`, `SPLIT` (compact drops the TX frequency), view chip, held TX offset (compact
drops `(HOLD)`), `FREQ: HOLD/AUTO`, `QSOs: n`, `DEC: preset`; then restore `MHz`, the full pause
text, the split frequency, `(HOLD)`, add the grid, restore the decode milliseconds, add
`Pancetta TUI`. #171's own-entity rule stays the final step: shown after the grid only if the grid
made it in and the entity still fits. When the row fits, every step succeeds and the row is
byte-identical to before. Rejected: "full form first" dropped the frequency at 80 columns under
load; "compact first" let `Pancetta TUI` crowd out `DECODE: AUTO 0ms` at 100 columns.

Columns: DX Hunter and Callers follow Band Activity's column-enum pattern. `visible_columns(width)`
walks a priority list, adding each column's minimum width plus 1 of spacing, and stops at the first
that doesn't fit (prefix rule: monotonic, so widening never removes a column). The budget is
`width − 2` borders `−` the 2-column `▶ ` highlight, reserved even when unfocused so the set doesn't
jump on Tab. DX Hunter priority: Call, Entity, SNR, Pri, Last, Rarity, Grid, Rpt, Src, Why (Operate panel
32 → Call/Entity/SNR; 40 → +Pri; 48 → +Last; 53 → +Rarity; 64 → the original nine; 89 → +Why).
Callers priority: Call, Msg, SNR, Flags, Age, Freq (32 → Call/Msg/SNR; 40 → +Flags; 48 → +Age;
53 → all six). Callers `Flags` narrowed 12 → 9 (`BUSY WKD` fits; the rare three-flag case reads
`BUSY MINE`). Empty states read `No DX` / `stations` and `No one` / `calling`.

Footer: the key hints use the same `fit_bar`; `?:Help` and `q:Quit` are pinned and
`Space:Call/Reply` shrinks to `Space:Call` first, which is exactly what 80 columns needs.

`?` overlay: the body scrolls by rendered line (↑/↓, PgUp/PgDn by a page, Home/End), clamped in
the key handler against the real terminal size (80×24 when there is none, as in unit tests) and
again in the renderer. Descriptions wrap at word boundaries with a hanging indent under the
description column. While the list can't fit, the footer reads
`↑↓ PgUp PgDn scroll · a–b of n · Shift+Q EMERGENCY STOP · Esc close`; when it fits, the overlay is
unchanged (`Press Escape or ? to close`). The scroll keys are modal-only, so per `keymap.rs` they
are documented by the modal footer, not `KEYBINDINGS`; `docs/KEYBINDINGS.md` is unchanged.
`App::help_scroll` resets to 0 every time the overlay opens.

PAN-89 moved the health alarm (audio dead, decoder stub, clock skew) out of the title bar into the
readiness strip, so the alarm segment and `title_rank::ALARM` are gone; the readiness strip doesn't
touch title-row fitting.

Known gap: `crossterm::Event::Resize` is unhandled and `App::handle_resize` is never called, so
`App::terminal_size` stays at its 80×24 default and mouse hit-testing maps clicks against 80×24
geometry on every terminal size. Separate defect, not fixed here. Out of scope too: vertical
layout tiers (idle TX-placement collapse, QSO Status/Hunt height tiers) and status-line truncation.
Tests: `bar_fit::tests`, `ui::view_render_tests::title_bar_*` / `*_headings_are_whole_words_*` /
`footer_key_hints_*`, `dx_hunter::tests::visible_columns_*`, `callers::tests`, and
`tui_runner::key_tests::help_overlay_*`.


## Readiness strip — durable blockers vs acknowledgeable warnings vs transient status (PAN-89, 2026-10-12)

`pancetta-tui/src/readiness.rs` (model), `pancetta-tui/src/ui/readiness_strip.rs` (render),
`pancetta/src/coordinator/readiness.rs` (snapshot + publisher). Supersedes the PAN-114 "title-bar
alarm slot" placement above.

**Three surfaces.** Row 1 of every view (and zoom) is the readiness strip: a TX verdict chip
(`… CHECKING`, `✗ TX HELD`, `■ RX ONLY`, `◌ TX WAIT`, `⚠ TX OPEN`, `✓ TX READY`) followed by one chip
per active condition, rebuilt every frame from typed `App` state. The status line stays the
transient-activity surface; decodes no longer write it (Band Activity lists them, `Msgs: N` counts
them, and the echo was what erased errors within a second of busy band). The Shift+D ring stays
history.

**Severities.** Blockers (red: PTT may still be keyed, rig control restarting, rig CAT link lost, TX
audio output dead, audio input dead) stay until the condition resolves; `!` cannot hide them.
Warnings (yellow: clock skewed/unverified, decoder stub, TX → system default, RX fallback, input
silent, RF-but-no-decodes, supervisor degraded, sticky error alerts) stay until resolved or
acknowledged with `!`. Notices (cyan: rig control not ready, rig command queued/applying) are
short-lived waits. Clock skew is now a yellow, acknowledgeable warning: it does not hold TX and a
non-dismissable red item would be permanent noise for a known-skewed portable station.

**Episode-scoped acks.** `App::prune_acknowledged_readiness` drops an ack as soon as its condition is
no longer active, so a recurrence shows again. The degraded ack is keyed by the summary text, so a
changed summary shows again. `TuiMessage::Error` (incl. startup config warnings) becomes a sticky
alert (dedupe by text while unacknowledged, ×N count, cap 8). Acks are session-only.

**Gate parity.** The verdict reads `StationReadiness.tx_mute_causes`, built by the coordinator's
display-only `tx::tx_mute_causes`, which lists every active cause in `tx_hard_mute_reason`'s order.
`tx_mute_causes_first_matches_tx_hard_mute_reason_for_every_input_combination` proves the first cause
equals the gate's reason for all 288 input combinations. `tx_hard_mute_reason` itself is unchanged.

**`ptt_off_unconfirmed`.** Latched exactly where Hamlib teardown retains the PTT watchdog because
PTT-off could not be forced; cleared only by a rig-accepted PTT-off (teardown success, the startup
unkey, the PTT watchdog's forced off, a command-loop `SetPtt{false}`). Orphan release is not a
confirmed unkey and does not clear it. The falling edge adds `PTT-off confirmed by the rig —
stuck-PTT warning cleared` to Shift+D. It does not mute TX: that is a TX-safety policy change for its
own ticket.

**Other findings.** `ui/station_info.rs`'s RIG / TX→default / RX→fallback badges have been unrouted
since Task 18, so those conditions were invisible in every view; the strip now carries them. The
Shift+S TX-audio row was inverted (green for the misconfiguration) and is fixed; Shift+S gains a
readiness section (acknowledged items marked `(ack'd)`), an RX-input row and a clock row. Because
the readiness section can outgrow the panel (at 80×24 about four items fit before the rig, audio and
clock rows clip), Shift+S scrolls by rendered line (↑/↓, PgUp/PgDn, Home/End), clamped like the `?`
overlay and reset on open; the bottom border shows `↑↓ PgUp PgDn scroll · a–b of n` only while it
overflows. Tests: `ui::view_render_tests::health_panel_scrolls_to_rows_clipped_at_80x24`,
`tui_runner::key_tests::health_panel_scroll_keys_are_clamped_and_reset_on_reopen`. The strip's row
also leaves Operate's right column 11 rows at 80×24, where the 20% station card is borders only and
Callers had no row under its header; below 3 card rows Operate drops the card and splits the column
50/50 between DX Hunter and Callers, as Hunt hides its card on short terminals. Larger sizes are
unchanged (`ui::view_render_tests::callers_shows_a_caller_row_at_80x24`). The supervisor's degraded summary is shared state sorted by component name instead of a 5 s overwriting
`StatusUpdate`.

**Out of scope.** Remote-operator arm/control presence (remote gateway sends stay additive-only and
the contract lives in dispensa), the SIGTERM/quit shutdown PTT-off path (runs after the TUI exits),
any change to the TX gate, per-command "saved / sent / confirmed / applied" tracking beyond rig
frequency/split, and persisting acks.

## Rig picker re-enumerates serial ports on open (2026-10-17, PAN-60)

**Problem.** PAN-59's `i` rig picker got its serial-port list once: the coordinator called
`serialport::available_ports()` in a one-shot startup block of the relay task, directly on the
async runtime, and never again. A rig plugged in while pancetta ran never appeared, so switching
rigs still needed a restart, the complaint PAN-59 was filed for. The `d` audio picker's startup-only
push was the model, but audio devices are present at boot and a newly plugged-in rig is not.

**Shape.** Opening the picker (`i`, closed→open only; `i` typed inside it is a Model character)
sends `TuiCommand::RequestRigConfig`. The relay arm calls
`pancetta/src/coordinator/tui_relay.rs::spawn_rig_config_refresh` and returns to `try_recv` at once.
The helper spawns a detached task that runs `enumerate_serial_ports` (the only `available_ports()`
call under `coordinator/`) through `spawn_blocking`, then takes the config read lock, builds the
`TuiMessage::RigConfigUpdate` from `cfg.rig`, and `try_send`s it while still holding the guard. The
startup push uses the same helper, so the runtime never blocks on enumeration and the command loop
starts without waiting for it. Single-flight: an `AtomicBool` claimed with `swap(true)` and released
by a `Drop` guard in the task (normal end, enumerator panic, or the runtime dropping the task at
shutdown); a request made during a refresh spawns nothing and is answered by that refresh's reply.

**Why this shape.** The relay loop also carries Shift+Q, StopTx, TogglePtt and AbortQso, and the
PAN-59 I4 rule (on `SelectRig`'s arm) forbids awaiting slow work inline; awaiting `spawn_blocking`
still suspends the loop. The two registered workers exist because queued disk writes must survive
shutdown; a read-only enumeration has no such need, so it follows `SelectRig`'s unregistered
`tokio::spawn`. Single-flight caps parked blocking-pool threads at one if a Windows serial driver
stalls SetupAPI, and keeps replies in order. Reading config after enumerating means a switch made
before a reopen is in the reply (the PAN-59 "no re-push after a switch" finding). Holding the read
guard across the send orders the reply against the bookmark worker, which commits under the write
lock and sends `RigBookmarksUpdate` after releasing it, so a stale bookmark list can never land
last. `try_send` drops the reply on a full TUI channel instead of parking a runtime thread under a
lock `SelectRig` needs to write.

**Edit-preserving merge.** The reply lands a frame or two after the picker opens, and a full apply
overwrites every field. `App::apply_rig_config_update` therefore merges when the picker is visible
and `RigSelectionState::has_operator_edits()` (any field differs from the snapshot taken on open, or
a bookmark sub-overlay is open): `merge_refreshed_ports` replaces the port list, re-finds the
selected and committed ports by name (prepending either if it vanished, as I-2a and PAN-61's I-4
do), replaces bookmarks with the overlay selection clamped, and touches nothing else. It also merges,
open or closed, a reply read before the coordinator handled the operator's latest `SelectRig`: the
relay counts handled `SelectRig`s (bumped under the config write lock when it applies one), the
refresh stamps that count on the reply as `select_seq` under its read lock, and the TUI merges any
reply whose `select_seq` is below `RigSelectionState::submits_sent`. Without it, a refresh read just
before Enter and drained afterwards (after the picker closed, or behind a decode backlog after it
reopened) put the old rig back in the form, and the next Enter re-applied it. Otherwise an untouched
open picker, or a closed one, takes the full apply, so a reopen after a switch the coordinator
refused shows the configured values. `active_field` is never touched.

**Accepted trade-offs.** No "scanning…" marker, no in-picker rescan key and no on-screen error: the
picker opens on the last-known list and is updated in place; Esc then `i` rescans; a failed
enumeration is a warn log and an empty list (the configured port is still offered). If the operator
closes, plugs in and reopens within one enumeration, the reopen is coalesced into the in-flight
refresh and the port shows on the next open. A hung `available_ports()` cannot be cancelled: the
flag stays set and the picker keeps the last-known list, which is the pre-PAN-60 behaviour. Tests:
the six `tui_relay_tests::rig_config_refresh_*` tests, the `app::tests::apply_rig_config_update_*`
merge tests, and `tui_runner::key_tests::rig_picker_reaches_a_port_plugged_in_after_startup` /
`rig_config_refresh_landing_mid_edit_keeps_the_edit` /
`rig_config_refresh_landing_after_{enter,reopen}_keeps_the_submitted_rig`.

## Needed grids and states in DX Hunter (PAN-86, 2026-10-18)

DX Hunter displays `+EM34` for a grid not worked on the **current operating
band**, across modes. It displays `United States - +AR` for a known US
state/territory not worked on **any band or mode**. Both tokens use `+` and the
existing bold needed-DXCC highlight; the glyph also conveys need without color.
Only the state token gets the new Entity-cell highlight. Stale indicators retain
the glyph in muted color, and selection retains reversed/bold styling. Existing
callsign markers and other panels keep their meanings.

Grid display uses the validated, uppercase four-character square in a five-cell
column, retaining the complete locator in the row data. Invalid or missing grids
show `---`. Narrow layouts omit the whole column. Grid need uses the current
operating band even for a network spot from another band; an unknown operating
frequency suppresses the grid indicator.

These are local **worked** indicators, not confirmed WAS credits or award
eligibility. The existing 56 validated state/territory codes remain distinct,
including DC and territories. State knowledge alone does not record a contact.
Historical seeding accumulates every valid US-related contact's state, so a
station moving from AR to CA leaves both worked. The most recent historical
state supplies the station label when no live value is already cached.

The coordinator injects its shared `CachedStationLookup` into the TUI. Before
every frame, the runner scans retained DX rows but performs need lookups only for
rows eligible for display: heard within 180 seconds and not our own callsign.
The derived map uses the current band and memory-only cache lookups. No draw
callback does I/O or retains a lookup lock. Existing rows lose their markers after
a completed contact without another decode or spot; restored band snapshots are
recalculated too.

Grid and state history have independent availability flags. Opening/reopening
history resets them, and each successful query publishes availability after its
additive seed, including an empty log. Loading or failed reads show known values
without claiming they are needed and log the failure through tracing. Unknown
states still have no suffix. A runner without a provider keeps plain state values.

Worked-cache updates retain the existing QSO-completion semantics: they do not
wait for independent ADIF/SQLite writers to succeed. Persisted history determines
status after restart. State learned by the upload subscriber's QRZ enrichment
updates knowledge and worked state for this session only; it does not rewrite the
already-delivered local log events. This feature adds no pre-contact lookup,
state priority bonus, configuration, or remote-protocol field.

## DX Hunter explains its ranking (PAN-94, 2026-10-18)

The Station card adds a `Why:` line, wide DX Hunter panels add a `Why` column,
and `w` opens the focused station's score breakdown. The card lists the tier's
reason, cautions, a portable-suffix hint, rarity, and when the station was heard.
The table omits rarity and heard-time clauses because those have separate columns.
Cautions include an already-worked contact, a failed QSO this session, and limited
or old network reports. They combine both score sources even when the other source
wins the displayed rank.

Portable suffixes are guesses. Every portable hint begins with `maybe` before
POTA/SOTA; activation language appears only in the detail paragraph, which says
it is not a confirmed park or summit activation and that bare callsigns are missed.
There is no new glyph and no activation badge. A grid reason stays hedged unless
PAN-86's prepared history status confirms it. Missing scores and stale rows have
explicit explanations instead of fabricated factors.

The Why column joins last at panel width 89. Narrower panels keep their existing
columns. Hunt reserves six rows for the Station card; Operate retains its existing
share. The card fits whole clauses, ending with an ellipsis when clauses are omitted.
The overlay wraps long lines and scrolls with Up/Down. `w` or Escape closes it;
bare `k` still aborts the selected QSO and Shift+Q still stops transmission.

The detail view lists every input with its source, the actual weights and
within-tier arithmetic, and whether the station relay or TUI scored the row.
The existing Pri column remains available; raw arithmetic lives in the overlay.
Callsign-pinned focus remains stable when a fresh score moves a station in the list.

## Needed entity only from a loaded cqdx.io list (PAN-93, 2026-10-22)

DX Hunter's `+` before a callsign, the station card's `needed` flag and the
remote `DecodedView.needed` mean one thing: the entity matches a prefix in
cqdx.io's most recently loaded needed-entity list. That is the per-band list
for the operating band when cqdx answered for that band, otherwise the
all-bands list. While no list has loaded, no row shows `+` and the card shows
`worked` or `new`. `!` (ATNO) and `▲` (new on this band, from the local log)
are unchanged. The scorer still uses a home/logged-prefix guess while the list
is unknown; see
[the priority-scoring decision](priority-scoring.md#needed-is-a-claim-with-availability-pan-93-2026-10-22).

For a per-band-DXCC-new row, the card's `Why:` line, the DX Hunter Why column
and the overlay's `Why:` line use one of three headlines:

- `needed entity (cqdx list)`: the loaded list names the entity.
- `maybe needed entity (cqdx list changed)`: the row was scored as needed, and
  the list loaded since says it is not. It lasts until the next decode
  re-scores the row.
- `maybe needed entity (cqdx list not loaded)`: no list has loaded, so the
  score used the guess.

The `w` overlay's "needed entity" row shows a value and its source:

| Case | Value | Source |
|---|---|---|
| List loaded | `yes` / `no` | `cqdx needed list, <band or all bands>, <age> old` |
| Row scored before the list changed | what the score used | `scored before the cqdx list changed (now yes)` / `(now no)` |
| No list, guess said needed | `unknown` | `cqdx list not loaded; scored as needed (not a home or logged prefix)` |
| No list, guess said not needed | `unknown` | `cqdx list not loaded; scored as not needed (home or logged prefix)` |

The Why text updates on the next frame after the list loads; `+` appears when
the station is next decoded, within one slot.

**Strip chips.** `⚠ CQDX NEEDS UNKNOWN` shows while a configured cqdx.io has not
delivered the needed-entity list: an outage, an empty or invalid token, or a
401. `⚠ CQDX GRID NEEDS UNKNOWN` shows when only the needed-grid list failed
with an error other than 404. Both are Warning severity (yellow), acknowledgeable
with `!`, and never change the TX verdict. The Shift+S detail names the clipped
error (at most 160 characters, never the token), then says either "pancetta
keeps retrying." or "Fix [network.cqdx] token and restart pancetta.", and what
is missing until the list loads. One outage gives one chip: when both lists are
unavailable only `⚠ CQDX NEEDS UNKNOWN` shows, and its detail ends with "Needed
grids are unavailable too." The chips clear at the next 2 s snapshot after the
list loads. Their keys carry no error text, so an acknowledgement survives a
changed error and is pruned on recovery; a later outage shows again. There is
no chip when cqdx is not configured, while the first fetch is in flight, or
when cqdx does not offer the needed-grid endpoint (404). Not configuring cqdx
is a choice, not a fault.

**No per-row unknown glyph.** An outage would put the same mark on every row
and say nothing about any one station. The strip chip and the Why text carry
"unknown" instead, as PAN-86 does for unknown grid and state history.
