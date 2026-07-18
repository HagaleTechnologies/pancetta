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
  commit: 6927e02c
  date: 2026-07-18
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

## A real instance of this bug (fixed 2026-07-18)

`respond_to_cq_with`/`respond_to_caller` used to leave `tx_parity` permanently
`None` when answering a station with no observed `dx_parity` (a DX-cluster/
DX-Hunter spot pick, not a live decode) — the TX scheduler then re-resolved
"nearest next slot" independently on every emission and the QSO alternated TX
windows, exactly the failure this page warns about. Fixed in the 2026-07-18
deep review's Batch 2 (`584dd81a`): the QSO now latches a provisional concrete
parity immediately and refines it to the true value on the first genuine
decode from the partner. Full findings: `docs/qso-tx-deep-review-2026-07-18.md`
(SM-F2/TX-F2).

## Where the invariant lives

- `pancetta-qso/src/qso_manager.rs:505` — `admit_new_qso` (idle adopts the side,
  same-side runs concurrent, cross-side queues, **never preempts**).
- `pancetta-qso/src/qso_manager.rs:534` — `current_tx_side` (the parity active
  QSOs are committed to).
- `pancetta/src/coordinator/tx.rs:3363` — `resolve_required_parity` (the
  scheduler resolves the actual slot).
- Manual cross-window picks defer into `PendingManualCalls` in
  `pancetta/src/coordinator/qso.rs`; as of 2026-07-18, `RespondToCaller` runs
  through the same admission gate too (it used to be the one manual entry
  point that admitted immediately regardless of the committed side).
- The TX coalescer and the autonomous multi-TX bundler both **exclude** (never
  coerce) a bundle stream whose `tx_parity` disagrees with the bundle's anchor
  — added 2026-07-18 after finding bundles previously just assumed their
  folded streams "naturally" agreed on parity, which nothing actually enforced.

Full digest and rationale: `docs/DECISIONS/tx-scheduling.md`; see
[[tx-scheduling]]. The completed-TX grace and defer windows are normative there
— do not restate them here.
