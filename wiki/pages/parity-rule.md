---
id: parity-rule
title: What will bite you about the half-duplex parity rule?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta-qso/src/qso_manager.rs
  - pancetta/src/coordinator/tx.rs
  - pancetta/src/coordinator/qso.rs
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - tx-scheduling
---
FT8 is deaf while transmitting, so **every concurrent active QSO must transmit
on the same parity** and we never TX in sequential 15s windows — the opposite
window must stay free to hear responses. If you add a TX path that keys the
wrong parity (or lets two QSOs straddle both windows), the station transmits
over its own ears and QSOs silently stall. This is the sharpest rule in the
scheduler; touch admission or coalescing without honoring it and you will break
on-air behavior that no unit test on a single QSO reveals.

## Symptom

QSOs advance to a report and then hang; the DX keeps re-sending because it never
hears your reply. Multi-QSO sessions are worst — one cross-parity admit poisons
the whole slot.

## Where the invariant lives

- `pancetta-qso/src/qso_manager.rs:505` — `admit_new_qso` (idle adopts the side,
  same-side runs concurrent, cross-side queues, **never preempts**).
- `pancetta-qso/src/qso_manager.rs:534` — `current_tx_side` (the parity active
  QSOs are committed to).
- `pancetta/src/coordinator/tx.rs:2201` — `resolve_required_parity` (the
  scheduler resolves the actual slot).
- Manual cross-window picks defer into `PendingManualCalls` in
  `pancetta/src/coordinator/qso.rs`, never keying the opposite window.

Full digest and rationale: `docs/DECISIONS/tx-scheduling.md`; see
[[tx-scheduling]]. The completed-TX grace and defer windows are normative there
— do not restate them here.
