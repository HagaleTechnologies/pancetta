---
id: receive-gain-sweep
title: What can invalidate a receive-gain experiment?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta/src/coordinator/gain_sweep.rs
  - pancetta/src/coordinator/gain_sweep_runtime.rs
  - pancetta/src/coordinator/pipeline.rs
  - docs/receive-gain-sweep.md
verified:
  date: 2026-10-30
links:
  - tx-scheduling
---

Attribute a receive-gain result to its source slot and immutable gain epoch,
not to the setting active when decoding finishes. Preserve negative-time
capture samples for the decoder but remove them before raw noise measurement.
A dropped pipeline window is missing evidence, never a zero-decode window.

Keep original PREAMP and ATT state with the coordinator-owned cleanup task.
An abortable worker future cannot own the only restoration copy. The coordinator
attempts PTT-off through the bus and direct rigctld safety paths before joining
cleanup, then restores gain before Hamlib teardown. Both control readbacks must confirm
restoration before a report can recommend a setting.

Fixed TX parity reserves those slots even without queued frames. Pending
TX intent skips gain admission until the request completes or is cancelled.
Idle slots with automatic parity remain eligible. Always read valid-window
and matched-station counts before treating
an inconclusive report as a band result. See the operator guide for hardware
qualification limits; synthetic tests do not establish on-air timing.
