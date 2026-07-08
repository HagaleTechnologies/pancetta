---
id: qso-engine
title: How does the QSO engine work and why is it shaped this way?
kind: subsystem
status: current
maintainer: agent
sources:
  - pancetta-qso/src/qso_manager.rs
  - pancetta-qso/src/autonomous.rs
  - pancetta-qso/src/exchange.rs
  - docs/DECISIONS/qso-engine.md
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - overview
  - tx-scheduling
---
The QSO engine is the state machine in `pancetta-qso` that advances a contact
through the FT8 ladder (CQ → grid → report → RR73 → 73), driven by decoded
frames and gated by sender verification. Each QSO carries a
`CallInitiation::{Manual,Auto}` flag that splits policy: manual calls bypass the
self-duplicate gate and keep-call under a watchdog; auto calls keep the
duplicate gate and yield to a busy DX. Thresholds and watchdog bounds are
normative in the code and `docs/DECISIONS/qso-engine.md` — cited, not restated.
Spec: `docs/superpowers/specs/2026-04-02-end-to-end-qso-design.md`.

## How it works

- **State transitions** — `determine_state_transition` / `is_message_relevant`
  in `pancetta-qso/src/qso_manager.rs` advance state only on frames that pass
  verification.
- **Sender verification** — every advance checks `from_station` == expected DX
  and frequency within tolerance; mismatches are logged (`target:
  "qso.security"`) and discarded. Origin: Security Review C-1/I-1
  (`docs/security-review-2026-04-29.md`).
- **Compound-callsign equivalence** — `exchange.rs::{base_callsign,
  callsigns_match}` treat a compound call and its bare base as the same operator
  mid-QSO, where WSJT-X/JTDX stall; conservative so impostors still reject.
- **Autonomous operator** — `autonomous.rs` classifies openings and drives Auto
  QSOs; OFF by default (the `a` toggle / `autonomous.enabled`).

## Why it is shaped this way

Manual and Auto diverge because an operator explicitly choosing a DX has
different duplicate/backoff semantics than unattended initiation (which also
carries an FCC-presence obligation). Admission and keep-call are still bound by
the half-duplex [[parity-rule]] and the [[tx-scheduling]] gate.
