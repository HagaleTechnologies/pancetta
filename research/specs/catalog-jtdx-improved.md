# Catalog: JTDX-Improved

## TL;DR

JTDX-Improved is real. It is maintained by Uwe Risse (DG2YCB), the same author
behind WSJT-X-Improved. It is primarily an automation / UI fork. As of the
reader date below, no decoder-algorithm improvements distinct from JTDX or
WSJT-X-Improved are advertised. The decoder code path is JTDX's, and any
"improved" changes that touch the decoder are the same ones already
catalogued in pancetta's existing `spec-jtdx-*.md` and `spec-wsjtr-*.md`
families (which trace back to JTDX and WSJT-X heritage).

- Reader date: 2026-06-08
- Reader: clean-room reader thread (this session)

## Identity

- Project name: JTDX-Improved
- Maintainer: Uwe Risse, DG2YCB
- Primary home: SourceForge — https://sourceforge.net/projects/jtdx-improved/
- Last update at reader date: 2025-12-25 (SourceForge listing)
- Activity: actively distributed (446 downloads in the week of the reader
  date; "Community Leader" badge with >50,000 cumulative downloads)
- License: GPL-3.0

## GitHub mirrors

- https://github.com/jj1bdx/jtdx_improved — a third-party clone by jj1bdx
  (Kenji Rikitake, JJ1BDX). Last push 2023-12-24, license listed as
  "NOASSERTION" on the repo metadata although the upstream COPYING is
  GPL. This mirror is stale relative to the SourceForge release line.
- Canonical sources of release artifacts remain on SourceForge.

## Relationship to other forks

- WSJT-X-Improved (also DG2YCB) is the older, more well-known fork.
- JTDX-Improved applies the same UX/automation flavor to the JTDX
  decoder line rather than to the WSJT-X decoder line.
- Upstream JTDX is the Chernikov / Järve project at
  https://github.com/jtdx-project/jtdx (and jtdx.tech), which is itself
  a fork of WSJT-X with its own decoder modifications (already
  catalogued in pancetta as `spec-jtdx-3method-sweep.md`,
  `spec-jtdx-qso-partner-filter.md`,
  `spec-jtdx-relaxed-sync-near-partner.md`).

## What it actually changes

Claimed/advertised improvements at reader date are all UI / workflow,
not decoder mechanism:

- Optimized GUI (option for either the wsjt-x_improved-style layout or
  the classic JTDX layout)
- Quick-access mode buttons (FT8, FT4, JT*)
- Band-hopping automation for FT8, FT4, and JT65
- Message highlighting by callsign, grid, or DX call
- Alert sounds on achievements (New DXCC, New Grid, etc.)
- JTAlert integration for callsign/grid highlighting
- QRZ.com lookup on double-click
- Dark stylesheet polish

No advertised changes to: Costas sync, LDPC/OSD, multi-pass subtract,
candidate generation, AP decoding, or the FT8/FT4 demodulator front
end.

## Decoder algorithm assessment

- Any decoder behaviour that differs from upstream WSJT-X is inherited
  from JTDX itself. Pancetta has already extracted the three known
  JTDX-specific decoder mechanisms (three-method magnitude sweep, QSO
  partner filter, relaxed sync near partner). Those specs cover the
  decoder-algorithm surface of the JTDX-Improved line.
- No new spec-worthy decoder mechanism was identified for JTDX-Improved
  beyond what is already catalogued.

## Recommendation

**No follow-on reader pass required for decoder algorithms.** The
DG2YCB JTDX-Improved fork is a UX/automation re-skin of upstream JTDX;
the decoder is JTDX's decoder. If a future investigation wants to be
thorough, the path is to diff the JTDX-Improved release tarball
against upstream JTDX at the same baseline tag and look for any
`lib/ft8/*.f90` or `lib/sync8.f90` deltas — but the advertised
feature list does not promise any such delta.

If the operator's interest is automation patterns (auto CQ, band
hopping schedule, achievement alerts), those translate to pancetta's
autonomous operator layer rather than to the FT8 decoder, and would be
catalogued separately under a `catalog-automation-patterns-*.md` if
the operator wants that.
