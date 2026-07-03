# QSO-engine bugs — triage (2026-07-03)

Two operator-reported live bugs in the autonomous/always-answer QSO path, both
root-caused against the running `main` checkout's logs and the code. **Triage only —
not yet fixed.** Both are in the always-on QSO engine, so they affect real on-air
operation regardless of the autonomous toggle.

---

## BUG 1 — Calling CQ transmits every window instead of alternating (half-duplex violation)

**Symptom (operator):** When we initiate CQ we transmit *every* 15 s window rather than
latching one parity and listening on the opposite. FT8 is half-duplex — keying every
window means the reply window is never free, so **we never hear anyone answering our
CQ.** We effectively call forever, deaf.

**Severity:** High. A CQ that can't hear replies never completes a QSO.

**Root cause.** A `CallingCq` QSO is created with **`tx_parity: None`**
(`pancetta-qso/src/qso_manager.rs` `start_cq` / `start_cq_manual`). `rearm_manual_
calls_at` re-emits a `Cq` `MessageToSend` **every 15 s slot** (`const SLOT_SECONDS =
15`). Each emission is resolved by `resolve_required_parity(None, tx_self_parity, now,
slot_ns)` (`pancetta/src/coordinator/tx.rs:2112`). With `tx_parity=None` and the
**shipped default `TxSelfParity::Auto`** (`pancetta-config/src/station.rs:188`), the
`Auto` arm returns the parity of the **nearest next slot** — i.e. "the next window,"
whichever it is. So a Cq is queued for every window in turn ⇒ **PTT keys on both
parities in succession ⇒ continuous TX ⇒ deaf.**

Why it slipped through: regular in-exchange QSOs alternate correctly because they
**latch** `tx_parity = opposite_of(dx_parity)` at QSO start (#39 half-duplex
scheduler + DX-slot-aware TX). Only the **CQ-initiation path** (`tx_parity=None` +
`Auto`) never commits to one side. A non-default `tx_self_parity = even`/`odd` would
mask the bug by fixing the parity.

**Fix direction (preferred):** latch a single parity when the `CallingCq` QSO is
created — pick Even or Odd once and store it as the QSO's `tx_parity` — so every rearm
resolves to that fixed parity and the opposite window stays free. Alternatives to
consider together: make `resolve_required_parity`'s `Auto` case **latch-on-first-use
per QSO** rather than re-pick-nearest; and/or gate `rearm_manual_calls_at`'s
`CallingCq` arm to the latched parity's slots.

**Test:** a `CallingCq` QSO must key PTT on **one** parity only and leave the opposite
window silent across ≥4 slots (`coord_sim`/`qso_manager`). Cover the autonomous
CQ-self path (`StartAutonomousQso` `start_cq`) — same `tx_parity=None` origin.

---

## BUG 2 — Answering a caller escalates our TX from `-NN` to `R-NN` every slot with no DX response

**Symptom (operator):** In a QSO with KJ5NJF, we progressed from the initial `-14`
signal report to the `R-14` report-ack **ourselves**, without having heard from the DX
in the interim.

**Severity:** High. We advance the on-air exchange prematurely, desyncing from the DX
(who is still an earlier stage) — the contact stalls or completes incorrectly.

**Evidence (log `~/.pancetta/logs/pancetta.log.2026-07-03`, QSO 237952e5 @ 1650 Hz,
us=K5ARH, DX=KJ5NJF):**

| Time | Event |
|---|---|
| 03:09:44 | RX `K5ARH KJ5NJF EM12` — KJ5NJF answers us with grid |
| 03:09:44 | We answer at step Report → TX `KJ5NJF K5ARH -14` (our report) |
| 03:10:02, :22, :42, 03:11:02… | We TX `KJ5NJF K5ARH R-14` **every slot** — no DX report received |
| 03:10:13 | RX `K5ARH KJ5NJF EM12` — DX is **still sending grid** (never copied our report) |
| 03:12:14 | RX `K5ARH KJ5NJF R-10` — DX *finally* sends a report (2+ min later) |

The `R-14` sends are the **per-slot keep-call rearm**, not decode-triggered (they fire
at regular ~15 s ticks; the only decode in between was our own transmission echoed
back). Sender verification was **not** the culprit — `determine_state_transition`'s
arms correctly guard `is_partner(from,dx) && is_us(to)`, and no `qso.security` warning
fired.

**Root cause (two interacting defects, both `pancetta-qso/src/qso_manager.rs`):**
1. **`respond_to_caller(ResponseStep::Report)` creates state `SendingReport {
   their_report: None, .. }`** (~L1407) and emits the opening `SignalReport (-NN)`. But
   at the Report step we've only sent *our* report and are **waiting for the DX's** —
   the correct state is `WaitingForReport`.
2. **`rearm_manual_calls_at`'s `SendingReport => ReportAck` arm** (the "FIX 4" path)
   re-emits an **`R-report` every slot** for any `SendingReport` QSO, **without checking
   `their_report`.** It was written for the legitimate mid-QSO case "we already received
   their report, re-ack with R." Combined with #1, a fresh caller-answer
   (`their_report: None`) immediately escalates `-NN → R-NN` with zero DX input.

**Fix direction (either or both):**
- In `rearm_manual_calls_at`, split the `SendingReport` arm on `their_report`: `None` ⇒
  re-emit our plain `SignalReport (-NN)` (keep-calling, awaiting their report);
  `Some(_)` ⇒ re-emit `ReportAck (R-NN)`. Preserves FIX-4 while stopping the premature
  escalation.
- And/or create the caller-answer-at-Report QSO in `WaitingForReport` instead of
  `SendingReport`.

**Test:** a caller-answer that sends `-NN` must keep re-sending `-NN` (never `R-NN`)
across ≥3 slots until a valid `<us> <dx> [R]-NN` from the DX arrives, then advance to
RR73 (`coord_sim`/`qso_manager`). Cover the autonomous respond path (same
`respond_to_caller` entry).

---

## Secondary (defensive) — self-decodes are not filtered

Separately noted while triaging BUG 2: the decoder decodes our **own** transmissions
(RX bleed / same-radio monitoring), and `process_message` runs **unconditionally on
every decode** (`pancetta/src/coordinator/qso.rs:2021`) with **no `from_station ==
our_callsign` self-decode filter.** It did not cause BUG 2 (the transition guards
rejected the echo; BUG 2 is rearm-driven), but a defensive "drop any decode whose
sender is our own callsign, before it reaches the QSO state machine" would remove a
whole class of self-advance risks cheaply. Worth adding alongside the BUG-2 fix.

---

*Both bugs were triaged read-only: `main`-checkout **logs** were read via
`~/.pancetta/logs/`, and all code was read in the working checkout. No fix has been
applied — these are queued for implementation.*
