---
id: language-rust
title: Why is pancetta written in Rust?
kind: decision-digest
status: current
maintainer: agent
sources:
  - docs/DECISIONS/ADR-001-language.md
  - pancetta-audio/src/manager.rs
  - pancetta-dx/src/lib.rs
  - Cargo.toml
verified:
  commit: 18c385c788e0c0f6ef945af9b2f11c9fb917d1e2
  date: 2026-11-12
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

## Rust maintenance gotchas

`AudioManager::stop()` is synchronous. It requests async task cancellation with
`JoinHandle::abort()` before stopping the stream. Dropping a handle detaches the
task; constructing a timeout without awaiting it does not cancel anything.

`DxError` holds a private `Box<DxErrorInner>`. Construct errors with
`DxErrorInner::Variant.into()` and inspect them through `error.kind()`. HTTP
errors still pass through `NetworkError`, which removes request URLs.

Workspace lint settings apply only to members with `[lints] workspace = true`.
The argument-count threshold is seven; exceptions belong on the specific
function with a reason, so new functions still receive Clippy diagnostics.
