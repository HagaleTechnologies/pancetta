---
slug: hb-040-time-range-close
mode: ft8
state: shelved
created: 2026-05-26T18:15:00Z
last_updated: 2026-05-26T18:15:00Z
branch: iter/2026-05-26-batch-12
parent_hypothesis: hb-040
wild_card: false
scorecard: n/a (bank hygiene; no code, no eval)
delta_vs_main: n/a
disposition: SHELVE hb-040 — resolved by hb-012 (batch 11) and mr-006 (batch 11). time_range stays a no-op field.
---

## Why close it

hb-040 proposed two paths for `Ft8Config::time_range`: plumb it through
to negative-time Costas search, or remove the dead field. Two batch-11
findings made both moot:

- **hb-012 (batch 11):** the curated corpus is 90 s continuous multi-slot
  captures decoded as one buffer; the Costas search scans `t0` from 0
  across the entire buffer, so signals at any interior timing offset are
  *already found*. Negative-time search only matters for the
  unrecoverable first-slot pre-recording case (no data exists).
  Operational-only value for the live ring-buffer path, and that path is
  unmeasurable in the harness.
- **mr-006 (batch 11):** the corpus survey recommended capturing future
  audio slot-aligned at the source (kiwirecorder cron) rather than
  building a decoder-side preprocessor — the cheap always-do.

wild-50's 0/96 (the original motivation) is corpus-specific — 2 outlier
WAVs per hb-025's audit — with ~0 composite impact even if recovered.

Remove the field as a pure cleanup later if appetite arises, but it's
small-surface and the cost of carrying it is essentially zero.

## Decision

**SHELVE** as no-op on pancetta's corpus. No code, no eval. Bank entry
updated; counter bumped at batch end.
