---
id: timing-history
title: What can invalidate a timing-history experiment?
kind: gotcha
status: current
maintainer: agent
sources:
  - pancetta-ft8/src/dt_history.rs
  - pancetta-ft8/src/decoder.rs
  - pancetta-research/src/chrono_replay.rs
  - research/experiments/2026-10-26-pan160-timing-history.md
verified:
  commit: 2420c36463d600cab78d69adbe121cfb6211ac12 plus PAN-160 working tree
  date: 2026-10-26
links:
  - modes
---

Legacy V2 history expires observations on recording. It can return an old
prior if no new sighting arrives. AP timing uses `prior_at` to check expiry
at lookup time, reject future/nonfinite observations, and count duplicate
slot timestamps once. Offline callers must query and record using the slot
clock. Wall-clock evaluation duration must not age one arm differently.

Native DT is the signed sample offset `(row - padding - 2) * spec_step`.
FT8 rows span 0.08 seconds. Do not add fractional refinement a second time
or mix ft8_lib DT with native DT without normalizing their conventions.

A callsign snapshot's size does not prove that a timing prior was used.
Inspect eligible senders, actual lookup hits, and extra LDPC attempts. Missing
SHA-matched baseline caches mean missing truth, not zero expected messages.
The PAN-160 preflight checks all original inputs before either arm can run.

The production coordinator does not attach this research timing history to
its native decoder. Native research results therefore do not establish
on-air station improvement. Both timing-history switches remain off by default.
