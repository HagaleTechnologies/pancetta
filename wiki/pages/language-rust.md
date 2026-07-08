---
id: language-rust
title: Why is pancetta written in Rust?
kind: decision-digest
status: current
maintainer: agent
sources:
  - docs/DECISIONS/ADR-001-language.md
verified:
  commit: eac7aab
  date: 2026-07-07
links:
  - overview
---
pancetta is written in Rust (ADR-001, Accepted). It was chosen for memory safety
without a garbage collector — predictable latency matters for real-time audio —
plus strong C FFI for hamlib and single-binary distribution. Full record,
including the Go/Python/C++ alternatives considered:
`docs/DECISIONS/ADR-001-language.md`.

## Digest

The trade-off settled: accept a steeper learning curve and longer compile times
in exchange for latency predictability, compile-time error catching, and easy
deployment. The alternatives table and full consequences are normative in the
ADR — this page only points there.
