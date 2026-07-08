---
id: modes
title: How do operating modes (FT8/FT4/FT2, Hound) work?
kind: subsystem
status: current
maintainer: agent
sources:
  - pancetta-ft8/src/protocol.rs
  - pancetta-core/src/slot.rs
  - pancetta/src/coordinator/dsp.rs
  - pancetta/src/coordinator/ft8.rs
  - docs/DECISIONS/modes.md
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - overview
  - tx-scheduling
---
pancetta runs a single station-wide operating mode — FT8, FT4, or FT2 — chosen
at startup (`[rig] mode`) and switchable at runtime with Shift+M. FT4 is the
faster (7.5s T/R, 4-GFSK) FT8 sibling; the codec already existed, so this is an
*integration* of slot timing + mode-stamping, not a new codec. Hound is an FT8
*operating procedure* (Tx≠Rx DXpedition chasing), not a distinct wire mode. Slot
periods, band dial tables, and offset regions are normative in the code and
`docs/DECISIONS/modes.md`. Specs:
`docs/superpowers/specs/2026-06-29-ft4-mode-design.md`,
`2026-07-05-runtime-mode-switch-design.md`, `2026-06-27-hound-mode-design.md`.

## How it works

- **Period-parameterized timing** — `pancetta-core/src/slot.rs` gained
  `_with_period` variants; `derive_dsp_timing` maps a `ProtocolParams` to
  window/decode-phase/overlap/slot values threaded into `dsp.rs` and the parity
  stamping in `ft8.rs`.
- **Runtime switch** — `try_switch_operating_mode` is the single gate; it
  refuses while any QSO is active (synchronous lock, zero `.await` in the
  critical section) and only then mutates the shared protocol + timing atomics.
- **Hound** — a manual `Tx≠Rx` procedure using `QsoMetadata.partner_freq`; mode
  stays FT8, flagged in ADIF.

## Key invariant

**`mode=FT8` must stay byte-identical** when FT4/FT2 are untouched — every path
resolves to today's exact FT8 constants (regression-guarded). Do not change a
slot or parity default without preserving that. See [[tx-scheduling]] for how
the active period feeds the scheduler.
