# Symptom C — Adaptive TX Coalesce Window — Design Spec

**Date:** 2026-07-21
**Status:** Proposed. Not started.
**Author:** Claude Sonnet 5 (under K5ARH supervision)
**Related:** `docs/qso-state-machine-analysis.md` (Symptom C, "multi-TX starts single-only in the
first window; multi by the second" — CONFIRMED, still open); `docs/qso-tx-deep-review-2026-07-18.md`
§"Symptom C residual gap"; deferred 2026-07-03 as its own design item (assistant memory
`project_symptom_c_multi_tx_deferred`, historical — the blocker described there about `tx_parity`
not being extractable pre-match does not hold up against the current code, see "Why this is
tractable now" below); queued follow-on `project_mid_tx_abort_restart_followon` (mid-TX abort +
re-key) is explicitly **out of scope** here — different risk footprint, sequenced after this spec,
not a dependency of it.

## Problem

`pancetta/src/coordinator/tx.rs`'s TX worker coalesces queued `TransmitRequest`s so that
same-parity openings started close together batch into one `MultiTransmitRequest` instead of
firing one-per-slot. The coalesce point sleeps a fixed `COALESCE_COLLECT_WINDOW_MS` (800ms,
protocol-scaled) after popping the head request, then drains whatever else has arrived.

Manual picks are produced by independent async round-trips (TUI keypress → command channel →
`StartQso` → `respond_to_cq_manual` → one `MessageToSend` → one `TransmitRequest`) at realistic
human pileup-working cadence: **1–3 seconds apart**, not within 800ms. Result: keypress 1 pops,
sleeps 800ms, drains — keypresses 2/3 aren't enqueued yet → single stream fires this window;
stragglers rejoin only via the next-slot keep-call rearm, one per cycle, until all are live.
Autonomous doesn't show this (`plan_slot_transmissions` emits all openings in one tight loop
between slots — all land within milliseconds already).

Confirmed still open as of the 2026-07-18 deep review: "structure unchanged (800ms first-pickup
window can't batch serial [keypresses])."

## Why this is tractable now

The 2026-07-03 deferral cited a blocker: reanchoring the window to the slot boundary needs
`required_parity` (via `resolve_required_parity`), which needs `tx_parity` from the message body,
which "isn't extractable until after the type-specific match arm... after the coalesce point the
wait needs to gate."

Checked against current code: `coalesce_backlog_into` (tx.rs:996) already destructures
`tx_parity` out of a `TransmitRequest` head via `if let`/`match` *before* any full type-dispatch —
same mechanism this design needs, already proven in production. `resolve_required_parity`
(tx.rs:3533) is a pure function of `(tx_parity, tx_self_parity, now, slot_ns)` — no active-QSO or
other state lookup. Peeking the target slot at head-pickup time, before deciding how long to wait,
is mechanically cheap. The 2026-07-03 note is superseded by this finding.

## Goal

Fix Symptom C's residual gap for the manual-pick path only. Explicitly out of scope: the
autonomous path (already correct), any on-air truncation mechanism, and the mid-TX abort/re-key
follow-on (a separate future spec that reduces the *need* for this kind of pre-PTT waiting via a
different strategy — start now, correct mid-flight — but this design does not depend on it, per
2026-07-21 discussion: keep the two efforts genuinely separate, this one stays zero-on-air-cost).

## Design

### 1. Adaptive collection window

Replace the fixed post-pickup sleep with an adaptive one, still gated on the head being a
`TransmitRequest`:

- Base wait: today's value, unchanged (800ms, protocol-scaled via `coalesce_collect_window_ms`).
- After the base wait, peek the channel (non-consuming where possible, or peek-and-requeue): if a
  `TransmitRequest` with the **same required parity** as the head arrived during the wait, extend
  by one more base-length increment and re-check. Arrivals of a *different* required parity do not
  extend the window — they target a different slot entirely and are handled by the existing
  per-item coalesce/drain logic unchanged.
- Hard cap per extension cycle: `min(configured_max_extension_ms, remaining_headroom_ms)`, where
  `remaining_headroom_ms = tx_late_max_ms - mstr_in_cur_slot_at(request_received_at)` for the
  head's resolved required parity, minus a small fixed safety margin (reserve enough that the
  *decision* computed off `request_received_at` — see §2 — still has room to act on). `configured_
  max_extension_ms` starts at a conservative small multiple of the base wait (exact value decided
  during implementation planning, not hardcoded in this spec) — the important invariant is that the
  cap is bounded by remaining `tx_late_max_ms` headroom, not a flat constant, so a request arriving
  late in its slot extends little or not at all — never worse than today's fixed 800ms for that
  case — while one arriving early in the slot has real room to batch.

This reuses `request_received_at` (already captured pre-sleep, per the existing Symptom B fix) as
the anchor for both the headroom computation and the eventual defer/viability decision — no new
timestamp concept for that part.

### 2. Timing-accuracy fix (folded in, not a separate follow-up)

Traced what happens downstream of `schedule_tx`: it's called once (Step 2, tx.rs:~1715), using the
frozen `request_received_at`, producing `cursor_offset_samples`/`silent_pad_samples` — how much of
the modulated waveform's front to trim so transmitted tones land on the real FT8 slot grid. That
computation implicitly assumes audio is sent immediately relative to its `now` argument. In
practice, for the current-slot case, `target_slot` is already in the past by construction, so
Step 6's `duration_until(target_slot, real_now)` sleep is a no-op — audio goes out whenever the
worker actually reaches Step 7, which is `request_received_at + (collection window actually taken)
+ encoding/gate overhead`. That gap is real, uncompensated lateness not reflected in the cursor
math baked in at Step 2.

Today this gap is small (~800ms + minor overhead) and evidently within decoders' DT search
tolerance — already shipped, already working. Widening the window per §1 would proportionally grow
the *same* pre-existing gap, up to several seconds under sustained same-parity arrivals — a real
decode-success risk at the receiving station, not hypothetical.

**Fix:** keep `request_received_at` for the defer/viability decision only (Symptom B's actual
concern — don't let the window itself tip a marginal request over the `tx_late_max_ms` cliff).
Recompute `schedule_tx` a second time, using a fresh clock read taken immediately before Step 5
(PTT key), and use *that* result's `cursor_offset_samples`/`silent_pad_samples`/`target_slot` for
the actual keying/audio steps. Any window-widening becomes a pure wait-to-batch cost with no added
timing-accuracy risk, and the existing 800ms path is quietly hardened as a side effect.

### 3. Interruptibility

The collection-window sleep is currently not interruptible (documented gap, deep review TX-F7) —
tolerable at 800ms, less so once it can run several seconds under §1's extension. Switch it to use
the existing `interruptible_sleep` helper (already used for every other wait in this worker) so F8
abort / shutdown cancel the window promptly instead of waiting it out.

### 4. Testing

This area is deliberately not wall-clock-tested by `coord_sim` (documented: "no schedule_tx UTC
math and no slot sleep... pass/fail never depends on wall-clock slot phase"). Add direct unit tests
for the new window-sizing logic, in the same style as the existing `schedule_tx_tests` table
(deterministic injected timestamps, no real sleeping):

- Extension triggers on a same-required-parity arrival during the base wait.
- No extension on a different-required-parity arrival.
- Cap correctly bounded by remaining `tx_late_max_ms` headroom for both an early-arriving head
  (room to extend) and a late-arriving head (little/no extension, no regression vs. today).
- The dual-`schedule_tx` call (§2) produces the expected `cursor_offset_samples` delta under a
  simulated processing delay, vs. the single-call (frozen-timestamp) result.
- F8/shutdown during an in-progress extension cancels within the existing ~50ms poll granularity.

### 5. Documentation

- `docs/qso-state-machine-analysis.md`: update the Symptom C section (currently "still open") with
  the resolution and this spec's link.
- `docs/DECISIONS/tx-scheduling.md`: append a dated entry.
- No `CLAUDE.md` invariant changes — this design doesn't relax "every transmitted frame reflects
  the freshest `MessageToSend` at key-time" (that's the mid-TX abort/re-key follow-on's concern,
  not this one's).

## Non-goals

- No on-air TX truncation or abort/re-key mechanism (separate future spec).
- No change to the autonomous TX path (`plan_slot_transmissions` already batches correctly).
- No change to `tx_late_max_ms` itself (mode-scaling it is a separately-flagged open question, not
  addressed here).
- No QSO-layer batching alternative (considered and rejected during brainstorming: it would just
  relocate the same collection-window problem to `StartQso`, duplicating logic rather than
  centralizing it in the worker where autonomous's equivalent batching already lives).
