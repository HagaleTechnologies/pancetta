# QSO state machine + TX timing — broad analysis

> **Superseded as of 2026-07-18** by `docs/qso-tx-deep-review-2026-07-18.md`, a broader
> re-audit whose findings are current and whose 5-batch remediation is fully landed
> (commits `6b2ceaf9`/`584dd81a`/`a43419df`/`e16b0370`/`6927e02c`). This file's own
> findings are historical: Symptom A (rearm coordination), Symptom B (collection-window
> timestamp), BUG 1 (CQ parity latch), and the GAP-1/GAP-2 early-close/rearm-rung arms
> were all fixed at the time this analysis was written (PRs #80/#81/#82, referenced
> below). Symptom C (multi-TX slow-start) was resolved 2026-07-21 — see
> `project_symptom_c_multi_tx_deferred` in the assistant's memory (historical) or
> `docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md` for the fix. For anything else, trust the newer
> review; this file is kept for historical context only.

A deep pass over pancetta's autonomous QSO handling: (A) state-machine correctness for
happy-path and edge-case QSOs, and (B) the TX-timing "jank" the operator observes
(redundant re-transmits, full-cycle start delays, multi-TX slow-start). **Analysis only
— nothing implemented.** Read alongside `docs/qso-engine-bugs.md` (two already-triaged
bugs this analysis generalizes).

## Executive summary

- **The happy paths are solid.** Both roles — us-CQing and us-answering — complete the
  full CQ → grid → report → R-report → RR73 → 73 ladder; every rung has both a
  transition arm and a reply arm, and the reports/grid latch correctly. Sender
  verification (`is_partner` + `is_us`) is present on every advancing arm and rejects
  spoofers with a `qso.security` warning.
- **The "jank" is real and has one dominant theme: the per-slot keep-call rearm is
  wall-clock-driven and does not coordinate with the response/advance path or with the
  TX collection window.** All three operator symptoms trace to timing coordination, not
  to the state machine's logic:
  - **Redundant re-TX** — a forward advance doesn't suppress that slot's keep-call
    rearm, so a late decode makes the rearm (old rung) and the advance (new rung) both
    target the same slot.
  - **Full-cycle start delay** — the 800 ms coalesce collection sleep is applied
    *unconditionally* before the slot is chosen, so a request arriving 7.2–8.0 s into a
    same-parity slot is pushed past the 8 s late cap and defers ~30 s.
  - **Multi-TX single-first-window** — the 800 ms collection window is anchored to the
    first pickup, but serial manual keypresses are produced >800 ms apart, so only the
    first pick is in the batch.
- **Edge-case gaps** (state machine): the biggest is that **autonomous (Auto) QSOs have
  no resilience to a dropped reply** — no keep-call, no regression handling — so they
  silently stall to timeout. Plus a missing early-close arm and a couple of low-severity
  items.

---

# Part A — State-machine correctness

Files: `pancetta-qso/src/{states.rs,qso_manager.rs,exchange.rs}`.

## A.1 Happy path — PASS (both roles)

- **Us-CQing:** `CallingCq` →(DX `CqResponse`, latch grid)→ `WaitingForReport` + reply
  `SignalReport` →(DX `ReportAck`)→ `WaitingForConfirmation` + reply `RR73` →(DX `73`)→
  `Completed`+log.
- **Us-answering:** `RespondingToCq` + emit grid →(DX `SignalReport`)→ `SendingReport` +
  reply `ReportAck` →(DX `ReportAck`)→ `WaitingForConfirmation` + reply `RR73` →(DX
  `RR73`)→ `Completed` + reply `73` + log.

Every rung has a transition **and** a reply arm; `their_report`/`our_report`/grid latch
correctly (`our_report` is held stable across regression). Reply firing is gated on
`new_state != old_state || is_manual_regression` and generated from the *same*
`(old_state, message)` pair that drove the transition, so transition and reply can never
disagree.

## A.2 Edge-case matrix

| Case | Verdict |
|---|---|
| DX skips a rung (bare report answering CQ; R-report at our grid; RR73 at our report) | HANDLED (A4 / skip-rung / FIX-2 arms) |
| DX repeats an earlier frame (regression) | HANDLED — **Manual only** (see GAP-3) |
| bare-call answer, no grid | HANDLED |
| early close from `WaitingForReport` / `SendingReport` | HANDLED (A5 / FIX-2) |
| **early close from `RespondingToCq`** (DX jumps straight to RR73/73 from our grid) | **GAP-1** — no arm, stalls at grid |
| compound ↔ base callsign mid-QSO | HANDLED (security-hardened) |
| two stations answer our CQ same window | HANDLED (first wins, 2nd no-ops) |
| wrong-station spoof | HANDLED (per-arm `is_partner`+`is_us`, `qso.security` warn) |
| our own TX echoed (`from == our call`) | WEAK — no explicit reject (GAP-5) |

## A.3 Ranked state-machine gaps

- **GAP-3 (moderate — autonomous) — Auto QSOs have no dropped-reply resilience.** The
  regression arms and the keep-call rearm are both gated `initiated_by == Manual`. An
  autonomous QSO whose single reply the DX doesn't copy gets **no re-send and no
  regression** — it stalls until the 30 s report/confirmation timeout retires it,
  silently losing QSOs the DX is still trying to complete. This is the highest-value
  state-machine gap for live autonomous operation: extend a **bounded** per-slot re-send
  (and regression handling) to Auto QSOs.
- **GAP-1 (moderate) — `RespondingToCq` has no early-close arm.** Arms exist for
  `CqResponse`, `SignalReport`, `ReportAck`, but not `(RespondingToCq,
  FinalConfirmation|SeventyThree)` — asymmetric with the A5 (`WaitingForReport`) and
  FIX-2 (`SendingReport`) early-closes. A DX that copies us on the first exchange and
  closes straight from our grid stalls until the watchdog retires it. Add the arm →
  `Completed`, mirroring FIX-2.
- **GAP-2 (low-moderate — jank) — rearm re-sends the wrong rung after stuck-at-grid.**
  `SendingReport` is entered two ways: the normal Caller path (`RespondingToCq` +
  `SignalReport`) after we sent an `R-report`, and the stuck-at-grid path
  (`RespondingToCq` + `CqResponse`, `their_report: None`) after we sent a plain
  `SignalReport`. But the rearm unconditionally re-emits `ReportAck` (R-NN), so a QSO
  that advanced via stuck-at-grid keep-calls a rung *ahead* of what it last sent. **This
  is the same defect family as `docs/qso-engine-bugs.md` BUG 2** (rearm's
  `SendingReport => ReportAck` arm ignores `their_report`). One fix closes both: gate
  the rearm on `their_report` (`None` ⇒ re-emit `SignalReport`; `Some` ⇒ `ReportAck`).
- **GAP-5 (low — defensive) — no explicit self-echo reject.** Nothing filters
  `from_station == our_callsign` before `process_message` (runs on every decode). Safety
  rests on the per-arm to/from checks + half-duplex physics. An explicit `from !=
  our_callsign` guard at the decode→QSO boundary would harden a whole class of
  self-advance risks cheaply.
- **GAP-4 (low — dead code) — `SendingConfirmation` is never constructed.** No arm
  produces it (the CQer's RR73 phase actually lives in `WaitingForConfirmation`); it
  carries vestigial `ladder_view`/accessor/timeout surface. Remove or mark reserved.
- **GAP-6 (very low) — CQer can't accept a two-rung skip** (caller answers our CQ
  directly with `ReportAck`). Rare; noted for completeness.

---

# Part B — TX timing "jank" (the three symptoms)

Files: `pancetta/src/coordinator/{tx.rs,qso.rs}`, `pancetta-qso/src/qso_manager.rs`,
`pancetta-core/src/slot.rs`. Defaults: `tx_late_max_ms = 8000`, `ptt_lead_ms = 80`,
`COALESCE_COLLECT_WINDOW_MS = 800`, `DELAY_MS = 500`. The decoder yields a DX decode only
~13 s into the DX's 15 s window. Two tasks touch a QSO: the decode loop
(`process_message` on every decode) and a 5 s ticker (`rearm_manual_calls` →
`check_timeouts`); they share `self.qsos` under a lock, so the hazard is *temporal*, not
a data race.

## Symptom A — "we retry the same message even though we received a response in time" — CONFIRMED

**Root cause: the keep-call rearm has no inbound-response suppression, and a forward
advance does not reset the rearm clock.** The rearm gate is pure wall-clock on
`last_call_at` (`elapsed >= SLOT_SECONDS(15)` → re-emit). `last_call_at` is stamped on
QSO open, on the rearm itself, and — critically — on the **manual-regression** branch
(with an in-code comment that this stamp exists so "the in-slot transition re-send and
the per-slot rearm never both fire in the same slot"). **A forward advance has no
symmetric stamp** (it only sets `progressed_this_cycle`, which the rearm ignores).

Concrete race (our parity Even :00/:30, DX Odd :15/:45; `last_call_at = :00`):
- `:15` — 5 s ticker fires. The DX's :15 response hasn't decoded yet (~:28). `elapsed =
  15 ≥ 15` → rearm re-emits the **old rung** targeting our :30 slot, sets `last_call_at =
  :15`.
- `:28` — DX response decodes → advance → auto-reply emits the **new rung**, also
  targeting :30.
- `:30` — two `TransmitRequest`s for the same `qso_id` exist for one slot.

Mitigations are incomplete: the **coalescer runs only at pickup** (the stale rearm frame
is popped at :15; the fresh advance arrives at :28 into a channel nobody is draining, so
they're never coalesced). The **Step-4c late-pivot** swaps stale text for the newest
intent — but only in the **single-`TransmitRequest` arm**; the `MultiTransmitRequest` arm
has no pivot, so in any concurrent/multi-QSO slot the stale rung transmits verbatim, and
the superseded request lingers to transmit next slot.

**Fix:** on a forward advance, stamp `last_call_at = message.timestamp` (or set a
per-cycle "advanced" suppressor the rearm honors), mirroring the regression guard — so a
slot in which we received a response suppresses that slot's rearm at the source.

## Symptom B — "we wait a full cycle (30 s) to start TX when we should start right away" — CONFIRMED

**Root cause: the 800 ms collection sleep is applied *unconditionally* before the target
slot is chosen, pushing a just-in-time request past the 8 s late cap.** The scheduler
picks the current slot iff `cur_parity == required_parity && mstr_in_cur_slot <=
tx_late_max_ms(8000)`, else the **next same-parity slot (~30 s away)**. The worker sleeps
`COALESCE_COLLECT_WINDOW_MS` before capturing `now` for `schedule_tx`. So a request whose
required parity equals the current slot's parity that arrives **7.2–8.0 s** into that
slot would take the "late but viable" skip-ahead branch and TX *this* slot without the
sleep — but with the 800 ms sleep `now` crosses 8000 ms → `use_current = false` → defer
~30 s. (The constant's own comment concedes this happens "rarely," not "never.") The
normal decode→respond path is immune — we TX opposite the DX and decode *during* the
DX's slot, so `cur_parity != required` → the immediate next (opposite) slot is chosen.
This bites the **manual / CQ-self** path where required parity == current slot parity.

**Fix:** capture `now` for `schedule_tx` *before* the collection sleep, or subtract the
elapsed collection time from the `tx_late_max_ms` comparison, so the window can never
itself tip a viable current-slot request over the cliff. (Making the sleep conditional
also addresses Symptom C.)

## Symptom C — "multi-TX starts single-only in the first window; multi by the second" — CONFIRMED

**Root cause: the collection window is anchored to the first pickup and is only 800 ms
wide, while serial manual keypresses are produced farther apart than that.** The
coalescer correctly batches whatever is in the channel at drain time; the 800 ms window
exists to let siblings arrive first. But each manual pick is an independent path (TUI
keypress → command channel → `StartQso` → `respond_to_cq_manual` → one `MessageToSend` →
one `TransmitRequest`), and the operator re-selects and presses per pick, so keypresses
are realistically >800 ms apart. The worker pops keypress-1, sleeps 800 ms, drains —
keypress-2/3 aren't enqueued yet → **single stream this window**; the stragglers land
after the drain and rejoin only via the next-slot keep-call rearm → "multi by the second
window." **Autonomous doesn't show it** (documented): `plan_slot_transmissions` emits all
openings in one tight loop between slots, so all N land within milliseconds, inside one
window. Widening the window trades latency for batching and still loses to a slow
operator.

**Fix:** don't rely on a fixed first-pickup sleep for the manual case. Either (a)
coalesce at the QSO layer — have the manual-multi `StartQso` path flush all selected
same-parity picks into one `MultiTransmitRequest` on the slot boundary; or (b) anchor the
collection window to the **slot boundary** (drain everything queued up to `slot_start −
ptt_lead`) rather than first-pickup + 800 ms, so any pick made during the current slot
batches into the upcoming TX. Option (a)/(b) also removes the unconditional 800 ms sleep
driving Symptom B.

**Resolved 2026-07-21** — see
`docs/superpowers/specs/2026-07-21-symptom-c-adaptive-coalesce-window-design.md` and
`docs/superpowers/plans/2026-07-21-symptom-c-adaptive-coalesce-window.md`. Implemented option
closest to (b) above: the fixed 800ms collection window now extends adaptively while the queue
keeps growing, capped by remaining `tx_late_max_ms` headroom and a protocol-scaled ceiling, instead
of a single fixed sleep. A folded-in timing-accuracy fix (refreshing the audio pad/cursor math
against real time immediately before PTT, rather than the frozen pre-coalesce timestamp) prevents
the wider window from growing an existing, previously-unaddressed audio-alignment drift.

---

## Cross-cutting synthesis + ranked fixes

Two mechanisms cause most of the jank:

1. **The keep-call rearm is uncoordinated** — wall-clock only, Manual-only, and
   `their_report`-blind. It underlies Symptom A (no advance-suppression), GAP-2/BUG 2
   (wrong rung after stuck-at-grid / caller-answer), and GAP-3 (no Auto equivalent, so
   autonomous stalls). A small rework of the rearm addresses all four:
   - suppress the rearm in a slot where we advanced (stamp `last_call_at` on forward
     advance) — **Symptom A**;
   - gate the re-emitted rung on `their_report` — **GAP-2 / BUG 2**;
   - extend a bounded rearm + regression handling to Auto QSOs — **GAP-3**.
2. **The 800 ms collection sleep is mis-anchored** — unconditional and first-pickup-based.
   Capturing `now` before the sleep / boundary-anchoring the window fixes **Symptom B**
   and **Symptom C** together and removes latency the lone-QSO path pays for nothing.

**Ranked fix list (highest leverage first):**
1. **Rearm coordination rework** (Symptom A + GAP-2/BUG 2 + GAP-3) — the single
   highest-value change; touches `qso_manager.rs` rearm + advance stamping.
2. **Boundary-anchor / pre-sleep `now` for the collection window** (Symptoms B + C) —
   `tx.rs` scheduling; also the cleanest fix for the multi-TX slow-start.
3. **GAP-1 early-close arm** for `RespondingToCq` — small, closes a stall.
4. **BUG 1 CQ parity latch** (`docs/qso-engine-bugs.md`) — independent but same
   subsystem; fold in.
5. **GAP-5 self-echo reject** + **GAP-4 dead `SendingConfirmation`** — low-cost hardening
   / cleanup.

Add `coord_sim`/`qso_manager` tests for each: advance-suppresses-rearm (one TX per slot
after a response), stuck-at-grid-keeps-`-NN`, Auto-QSO-re-sends-bounded, manual-multi-
same-window-batches-first-window, current-slot-manual-TX-not-deferred-30s, and the GAP-1
early close. Every behavior change gates through the existing `coord_sim` rig-level
harness.
