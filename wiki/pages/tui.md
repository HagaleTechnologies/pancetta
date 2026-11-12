---
id: tui
title: Why is the TUI shaped the way it is?
kind: decision-digest
status: current
maintainer: agent
sources:
  - docs/DECISIONS/tui.md
  - pancetta-tui/src/app.rs
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - overview
---
The TUI was redesigned (2026-07-03) into four task-focused activity views
(Operate/Hunt/Run/Monitor) with a vacancy-first TX-placement instrument
replacing the waterfall, a global callsign focus shared across panels, and mouse
support. It is shaped this way to make multi-QSO placement and station focus
visible — the old single-TX-marker waterfall could not show concurrent streams.
Full digest and the per-key changes: `docs/DECISIONS/tui.md`; spec
`docs/superpowers/specs/2026-07-03-tui-redesign-design.md`.

## Digest

The load-bearing rule to know before touching it: the **single-scorer
invariant** — the TX-placement display is computed from the *same*
`SmartFrequencyAllocator` path the autonomous operator actually decides with, so
the display never diverges from what the station would pick. The one feature
that autonomously writes a live TX atomic (opt-in auto-repark) is gated by a
5-case safety contract and fails closed. Constants and the full key map are
normative in the digest — not restated here.

Key handling in every scope is `keymap::resolve` plus `match action`: every bound key
resolves through its `KEYBINDINGS` or `MODAL_BINDINGS` chord, or a text-entry rule, into one `Action`. A
table test fails when a chord is ambiguous, unreachable, unnamed by its row or
Ctrl-reachable for a TX action. `tests/fixtures/key_effects.golden` fails a
test whenever what a key does changes. See
[the PAN-327 decision](../../docs/DECISIONS/tui.md#key-dispatch-through-the-keymap-table-pan-327-2026-11).

A bare `k` aborts the selected QSO at once, with no confirmation (PAN-263,
which removed the PAN-121 confirm modal). See
[the PAN-263 decision](../../docs/DECISIONS/tui.md#bare-k-aborts-at-once-no-confirmation-pan-263-2026-11-03).

DX Hunter's `+` on Grid means unworked on the current operating band; `+` on a
known US state/territory means unworked across bands. A pre-frame refresh reads
the shared cache only for display-eligible rows, excluding spots older than 180
seconds and our own callsign. Existing rows update without another spot and
restored band rows do not carry stale flags. Unknown history suppresses these
indicators.
See [the PAN-86 decision](../../docs/DECISIONS/tui.md#needed-grids-and-states-in-dx-hunter-pan-86-2026-10-18)
for history availability, state accumulation, and session-only QRZ enrichment.

PAN-94 adds a Station-card reason, a Why column at panel width 97 (since
PAN-345's Band column), and a `w`
breakdown overlay. Reasons use the inputs stored with the latest score, and
portable callsign suffixes are explicitly guesses. See
[the PAN-94 decision](../../docs/DECISIONS/tui.md#dx-hunter-explains-its-ranking-pan-94-2026-10-18)
for clause order, missing-history wording, score-source selection and keys.

Gotcha (PAN-93): `CachedStationLookup::is_needed_dxcc` is the scoring guess.
While cqdx.io's needed list has not loaded it still returns true for any call
outside the home and logged prefixes. Display and wire surfaces must use
`display_needed_atno` or `needed_dxcc_status`, which never claim needed while
the list is unknown. See
[the PAN-93 decision](../../docs/DECISIONS/priority-scoring.md#needed-is-a-claim-with-availability-pan-93-2026-10-22).

Gotcha (PAN-345): `DxStation::frequency` has no single unit. Local decodes and
cqdx rows store MHz, DX-cluster rows store Hz. Never infer a band from its
magnitude: read `DxStation::band`, which each source derives from its own unit
(cluster Hz, cqdx `frequency_hz`, decode MHz times 1,000,000) through
`pancetta_core::Band::from_frequency`. DX Hunter's `Band` column (five cells,
right after `Call` in width priority) renders it, `---` when unknown. Cluster
spots are filtered by `[network.dx_cluster.filtering]` in
`pancetta-dx/src/cluster.rs` before they reach the bus.

The local relay remembers the last decoded grid per exact callsign and band for
its session. Grid-less reports and acknowledgments use that grid when rescoring;
worked-grid and failure history are still read afresh. Otherwise a report could
erase the grid-based rank while DX Hunter still displayed the earlier CQ's grid.

Space on a network spot (PAN-287): a DX Hunter row from the DX cluster or
cqdx.io that was never heard locally is called through
`TuiCommand::CallNetworkSpot`, not `CallStation`. The TUI resolves the spot's
standard FT8/FT4 dial and audio offset from `DxStation.spot_rf_hz` (never
`frequency`, whose unit differs by source), and the coordinator QSYs first,
waiting for PTT-off. Gotcha: the TUI switches its band only when the
coordinator reports `TuiMessage::SpotQsy`, so a refused or parked call leaves
the band alone. See
[the PAN-287 decision](../../docs/DECISIONS/tui.md#space-on-a-network-spot-qsys-the-rig-pan-287-2026-11).

<<<<<<< HEAD
Replay sessions show `TX: REPLAY` in neutral gray from the first title-bar draw,
regardless of TX policy. The session flag is supplied by the coordinator and is
not saved in TUI config. See
[the TX-policy banner decision](../../docs/DECISIONS/tui.md#tri-state-global-tx-policy).
=======
PAN-351: transient statuses expire independently of readiness alerts. Pass an explicit severity through the bus and use Relay for service feedback. Local feedback may temporarily cover a held error. Do not restore decode echoes or use transient expiry to clear readiness.
>>>>>>> 27fba49d41c56da94ef46c7f9220424a86fbcf10

Gotcha (PAN-350): Band Activity reuses DX Hunter's prepared need verdict, but
only marks a grid when both rows have the same canonical square. A later rover
locator must not mark an older decode's grid. Call/Grid use 12/5 cells; the full
and middle width tiers start at 77/64.

Display vocabulary lives in `pancetta-tui/src/presentation.rs`. Rig, reply-step,
PTT and view labels are separate from Debug and persistence strings. Rarity
cells use words in a nine-cell column. Band Activity uses integer kilometres
under `km`; station-card prose appends the unit. TX, budget-cut and pause
markers are the ASCII characters `*`, `!` and `|`.

For display regressions, render actual buffers and concatenate complete cell
symbols. Check positive content in a direct panel as well as scanning full
views, since a hidden column or clipped overlay can hide a broken label.
`pan353_rendered_text` covers labels, action messages, distance placement and
marker widths. Its scanner is for controlled fixtures, not arbitrary radio text.

PAN-354: operator offset controls follow the allocator default range, but Auto
creation and active relocation retain distinct bounds. A held endpoint must
survive manual creation before collision avoidance. Placement Assigned means
current ownership, not quiet duration. Short single-QSO layouts admit complete
lines before rendering. See [the PAN-354 decision](../../docs/DECISIONS/tui.md#honest-qso-values-and-operator-offsets-pan-354).
