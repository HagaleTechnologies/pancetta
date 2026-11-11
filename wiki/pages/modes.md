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
- **Hound** — a manual `Tx≠Rx` procedure flagged by `QsoMetadata.hound`; mode
  stays FT8 and is flagged in ADIF. `partner_freq` supplies the Fox's
  receive location but is a generic split-TX field, not the Hound discriminator.
  Since PAN-259 it follows WSJT-X: call at 1000–2700 Hz, send the R-report at
  the Fox's exact offset (pinned there afterwards), log on RR73 with no 73.
  The decoder reads active Fox calls from the supervisor's current QSO manager
  each window. It seeds hashes again after a decoder rebuild, preserving
  collision evidence. Both receive backends keep the raw type 0.1 contacts;
  display and relay feeds still get one compound frame. Unknown or collided
  Fox hashes cannot advance a QSO. Hounds widen the ordinary receive mask
  across the configured Fox response envelope and use the standard sync
  threshold there, so a new stream is not restricted to the old offset.
  Report-driven moves take the QSO-map lock before the shared PTT gate and
  retain both through epoch invalidation and reply publication.

The autonomous `SlotTicker` in `pancetta/src/coordinator/autonomous.rs` remembers
the previous poll time. When it observes a mode change, a current-mode boundary
in `(previous poll, current poll]` triggers one immediate decision tick, even if
the old deadline is still ahead. This avoids skipping a boundary that passed
inside the roughly 250 ms polling gap. The next deadline stays on the new mode's
future UTC grid, with no catch-up burst. The ticker has no mode-switch timestamp,
so a switch just before or just after that boundary receives the same recovery.

## Key invariant

**`mode=FT8` must stay byte-identical** when FT4/FT2 are untouched — every path
resolves to today's exact FT8 constants (regression-guarded). Do not change a
slot or parity default without preserving that. See [[tx-scheduling]] for how
the active period feeds the scheduler.

<<<<<<< HEAD
## Companion UDP sampling

PAN-349 samples the live mode when draining decodes: FT8 uses `~`, FT4 uses
`+`, and FT2 is suppressed. Capture-time mode provenance remains unavailable.
Status samples the current QSO manager through its restart watch each time,
so replacing the manager cannot retain a contact from the old instance.
Selection is earliest active named contact of the current mode, then QSO ID.
RX DF uses `partner_freq.unwrap_or(frequency)`, which is an audio offset;
TX DF uses the station's held offset. They can differ. TX text comes only from
an accepted intent for that selected contact, and does not prove transmission.
FT4's integer Status period is 7 seconds; its scheduler still uses 7.5 seconds.
=======
## Rig CAT mode is separate

PAN-276 adds optional `rig.data_mode` and `rig.data_passband_hz`. These do not
select FT8/FT4/FT2. They request an exact CAT sideband/data mode at connection
and after commanded retunes. See `docs/CONFIG.md` for accepted values and split
limits. Omission preserves manual mode control.

A rigctld command can reconnect internally. Mode readiness must therefore be
bound to the actual connection generation inside the client, rather than only
set during coordinator startup. Key-down must check that generation and must
never reconnect and replay an old request. Hardware qualification is pending.
>>>>>>> 88af8930a9594ad8179ed41c213be781fcc1da85
