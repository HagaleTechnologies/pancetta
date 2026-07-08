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
