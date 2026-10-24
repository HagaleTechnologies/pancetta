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

DX Hunter's `+` on Grid means unworked on the current operating band; `+` on a
known US state/territory means unworked across bands. A pre-frame refresh reads
the shared cache only for display-eligible rows, excluding spots older than 180
seconds and our own callsign. Existing rows update without another spot and
restored band rows do not carry stale flags. Unknown history suppresses these
indicators.
See [the PAN-86 decision](../../docs/DECISIONS/tui.md#needed-grids-and-states-in-dx-hunter-pan-86-2026-10-18)
for history availability, state accumulation, and session-only QRZ enrichment.

PAN-94 adds a Station-card reason, a Why column at panel width 89, and a `w`
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

The local relay remembers the last decoded grid per exact callsign and band for
its session. Grid-less reports and acknowledgments use that grid when rescoring;
worked-grid and failure history are still read afresh. Otherwise a report could
erase the grid-based rank while DX Hunter still displayed the earlier CQ's grid.
