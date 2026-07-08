---
id: overview
title: pancetta — what is this and where do things live?
kind: overview
status: current
maintainer: agent
sources:
  - README.md
  - CLAUDE.md
  - Cargo.toml
  - docs/ARCHITECTURE.md
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - tx-scheduling
  - qso-engine
  - remote-operation
  - modes
---
pancetta is an autonomous FT8/FT4 ham-radio station in Rust: it decodes,
selects and calls stations by priority, completes QSOs on multiple TX streams,
and logs them — with integration to the first-party cqdx.io service. It is a
12-crate Cargo workspace targeting a Yaesu FTdx10; the authoritative record of
current behavior is the code plus `docs/superpowers/specs/`, with subsystem
decision digests in `docs/DECISIONS/`. This wiki points at those; it never
restates constants or thresholds.

## Subsystem map

- **TX scheduling** — slot/parity scheduling, coalescing, drop-stale-TX gate.
  See [[tx-scheduling]] and the sharpest rule, [[parity-rule]].
- **QSO engine** — state machine, manual vs. autonomous, sender verification,
  compound-callsign equivalence. See [[qso-engine]].
- **Remote operation** — read-only gateway + the armed station agent (the final
  TX authority). See [[remote-operation]], [[fail-closed-arm-gate]],
  [[additive-only-gateway]].
- **Operating modes** — FT8/FT4/FT2 + Hound. See [[modes]].
- **TUI**, **logging/uploads**, **config & platform** — see the decision-digest
  pages [[tui]], [[logging-uploads]], [[config-and-platform]].
- **Why Rust** — [[language-rust]].

## Where things live

- `pancetta/src/coordinator/` — the orchestrator (decode→decide→transmit).
- `pancetta-qso/` — QSO state machine, priority scoring, autonomous operator.
- `pancetta-ft8/` — encoder/decoder/OSD (bit-exact with WSJT-X).
- `pancetta-agent/` — offline safety core for remote TX.
- `docs/superpowers/specs/` — design specs, authoritative for current behavior.
- `docs/DECISIONS/<subsystem>.md` — per-subsystem decision digests.
- `docs/ARCHITECTURE.md`, `docs/CONFIG.md`, `docs/fcc-part97-compliance.md`.

## Start here

Read [[tx-scheduling]] and [[qso-engine]] first — the parity rule and the QSO
state machine constrain almost everything else. If you touch remote TX, read
[[fail-closed-arm-gate]] before anything else.
