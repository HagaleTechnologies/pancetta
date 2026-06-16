# QSO Adversarial Scenario Catalog — 2026-06-16

Synthesized from three sources: (1) **log analysis** of all past runs (`~/.pancetta/logs/*`),
(2) **peer/forum research** (WSJT-X/JTDX/MSHV/ft8mon/JS8Call + Hinson FT8 guide + wsjt-devel),
(3) **operator-grounded ideation** (50 scenarios). Built against the sim harnesses
(`pancetta-qso/src/sim.rs`, `pancetta/tests/coord_sim.rs` patterns, `Sim::inject_signal` hi-fi).

Goal: a permanent adversarial regression library that PROVES proper QSO progression — and
surfaces any case where a real on-air frustration would still happen.

## ⚠️ KEY LOG FINDING — likely real bug ("stuck-at-grid", highest frequency in corpus)
Across many real attempts (N8ME, F5NNN, N9FME, IQ0VT, KB5YNF, KA0NC, first-K9HJZ), we
answered with our grid (`DX K5ARH EM10`) and **kept re-sending our grid 10× until the
watchdog timed out**, even while the DX was responding (bare call or their grid). We only
ever advance to a report on a specific message shape; when the DX's "answer" doesn't match
that shape we never step grid→report. **Investigate via harness; if confirmed, fix the
state machine to advance to a report when the DX returns our call (bare or with grid).**
Log totals: 65 QSO ids, only 7 completions (5 stations; VB7F logged 3×), 16 full-10-call
timeouts, 13 supersedes — most stalls trace to this + tail-end re-call churn.

## Batch A — Completion / asymmetry / close-token semantics
- A1 stuck-at-grid: DX answers with bare `DX K5ARH` → we must send a report, not loop grid. **[BUG?]**
- A2 stuck-at-grid-2: DX answers `DX K5ARH <grid>` → we must send a report. **[BUG?]**
- A3 they 73 us while we're at report (skip R-report) → accept + log, stop sending report.
- A4 DX skips grid (answers CQ with `R-xx`) → accept, close, log (grid empty). **[FIXED]** — `(CallingCq, SignalReport→WaitingForReport)` arm + routing + report reply.
- A5 out-of-order RR73 before we reported → accept completion, log, send 73. **[FIXED]** — `(WaitingForReport, RR73/73→Completed)` early-close arm (CQer mirror of FIX-2) + routing + 73 reply.
- A6 wrong report value repeated (no RR73) → re-send our R, don't advance.
- A7 DX corrects report (different value 2nd time) → latch newest received report, re-send R.
- A8 stuck loop: neither copies R-frame → keep-call to watchdog, never auto-complete.
- A9 mutual deadlock: after we (CQer) receive their R, WE owe RR73 → must send it, not idle.
- A10 premature self-completion: single sent grid, no replies → never log; watchdog→Failed.
- A11 RR73 vs RRR vs 73 distinct close paths (RR73=no reply expected; RRR=expect 73). [peer D1]
- A12 log on OUR state, not string-match "73"; partner goes silent/CQs after report → still log. [peer A6/D7]
- A13 bounded auto-73 actually STOPS after the cap on repeated RR73 (verify the bound).

## Batch B — 3rd/4th-party + sender/to-field discrimination + yield-to-busy
- B1 DX working someone else: `Z DX -07` (to≠us) → recognize, yield, don't take their report. **[log pattern 6]**
- B2 DX picks the other guy: we answered, DX sends `X DX -10` → yield + back off.
- B3 third station calls mid-QSO (`K5ARH X ...`) → defer X, keep current DX QSO.
- B4 tail-ender pounces as we finish → complete DX, then start fresh QSO with X.
- B5 two answer our CQ same slot → deterministic pick-one (priority), handle/ignore other. [peer B1]
- B6 directed-CQ off-target answerer → policy (no auto-resp / configurable). [peer B2]
- B7 calling a station already in QSO (busy) → tail-end/queue, don't barge. [peer B3]
- B8 third-party exchange (`X Y RR73`) → no auto-reply, no advance. [peer B5]
- B9 impostor: same expected text from wrong from-call → no advance (sender verify). [peer B4]
- B10 station calls us with partner's call by mistake (`K5ARH DX` from X) → discard.
- B11 near-miss callsign answering our CQ (`K5ARG` vs `K5ARH`) → exact match, no false start.
- B12 multi-stream: N parallel QSOs don't cross-contaminate partner/parity/report. [peer A9]
- B13 foreign frame on our exact freq w/ DX call but wrong to → callsign+to+state, not freq alone.
- B14 we QSY mid-QSO (freq occupied) → same QSO continues on new TX freq, identity preserved.
- B15 DX drifts > tolerance mid-QSO → prefer callsign+state continuity for an active QSO. **[FIXED]** — `is_message_relevant` applies the freq gate AFTER the callsign/to/state match; an ESTABLISHED QSO (contra known) allows up to 100 Hz drift, tight 15 Hz kept for initial/ambiguous matching (gate widened, not re-latched).

## Batch C — timing / fading / lifecycle races + re-engagement + operator
- C1 DX fades after our report, returns 3 slots later → keep-call across silence, resume. [peer C2]
- C2 intermittent decodes (every other slot) → completes; watchdog counts calls not slots.
- C3 watchdog expiry vs just-in-time answer SAME slot → process RX before timeout; don't retire. **[FIXED]** — a forward state advance sets `progressed_this_cycle`; the manual watchdog grants a one-pass reprieve (and clears the flag each pass) so a just-in-time answer at the call cap is not retired in the slot it advanced. NOT a `call_count` reset (per-QSO cap preserved, C12); a regression clears the flag so a stuck DX still retires.
- C4 abandoned (watchdog-Failed) station starts answering within a window → re-engage fresh. **[decide window]**
- C5 we abandoned them but they close with us → accept gift completion + log.
- C6 late return after minutes (mapping cleared) → new QSO / dupe policy; no crash.
- C7 stale close after working another station → don't misroute into the new QSO; ignore/bounded.
- C8 return after band change → ignore close that doesn't match current band/freq context.
- C9 band change mid-QSO → tear down gracefully; no stale keep-call TXing on new band. **[FIXED]** — a real band change (different ham band, or ≥100kHz out-of-band move per `is_band_change`) fires `QsoMessage::BandChanged` → cancels active QSOs (purged from active_tx_qsos → stale TX dropped) + operator status. **All three trigger sites now wired**: (1) TUI `SetFrequency` (`tui_relay.rs`), (2) hamlib **dial-poll loop** (`hamlib.rs` — operator turns the rig dial; the poll observes the new freq and tears down), (3) autonomous **ChangeBand** (`autonomous.rs` — operator QSY). Dedup: each pancetta-initiated change (TUI/autonomous) stamps a `last_freq_command` (target_hz, instant) anchor; the poll loop suppresses its own teardown when the observed band is `band_change_attributable_to_command` — i.e. it matches the commanded band (settled) or the command is within the `FREQ_COMMAND_SETTLE_MS` (3s) rig-slew window (covers a transient old-freq reading). So a TUI/autonomous change is torn down exactly once, never re-fired when the poll reads the new freq back. Tests: `coord_robustness.rs` (`c9_dial_poll_band_change_tears_down_once`, `c9_dial_poll_does_not_double_fire_pancetta_initiated_change`, `c9_autonomous_change_band_tears_down`, `c9_band_change_attributable_to_command_matrix`).
- C10 DX on our own parity → re-derive tx_parity from DX frame; never key DX's slot.
- C11 late-in-slot decision → defer to next same-parity slot (don't TX truncated).
- C12 per-step-stall vs per-QSO watchdog semantics — clarify + assert.
- C13 operator re-click active station → continue, no duplicate. [covered — assert]
- C14 operator abort then re-call → clean teardown + fresh manual QSO.
- C15 operator manual frame out of sequence (force RR73) → honor override, log consistently.
- C16 operator clicks NEW station mid-QSO → queue or multi-stream; never silently drop/collide.
- C17 hashed/partial `<...>` callsign → no auto-reply, no advance, no log vs unresolved. [peer D2/D3]
- C18 compound call /P /R /MM consistent across frames; compound↔base equivalence (no stall). [peer D4] **[FIXED]** — base_callsign()/callsigns_match() (strip one prefix/suffix, compare base) used in all sender-verify + relevance arms; validate_callsign widened to accept compound; near-miss calls still rejected; logged under most-complete form.
- C19 config hot-reload mid-QSO must not clobber latched partner/parity. [peer A8] **[FIXED/GUARDED]** — no live reload path reaches QSO state today (holds by construction); added classify_config_reload() so any future apply-handler defers station.callsign/grid/parity while a QSO is active (ui/network/audio/rig safe-live).
- C20 RF-present-but-zero-decodes health signal (mode/clock fault). [peer D8] **[FIXED]** — RfNoDecodeMonitor: ≥4 consecutive slots with RMS≥floor (RF present) but zero new decodes → TUI warning 'RF present but no decodes — check mode/clock?'; quiet band never warns; edge-triggered.

## Phase 5 — autonomous QSO loop status (2026-06-16)

**Engine: DONE and sim-proven.** The QSO engine now drives `CallInitiation::Auto`
QSOs through the full reply ladder and retires unanswered pounces
(`13d423dc`). The 4 Phase-5 scenarios in
`pancetta-qso/tests/autonomous_scenarios.rs` (full exchange to completion,
context-aware skip-rung reply, RR73/RRR close, unanswered-pounce retirement)
pass driving the **real** `AutonomousOperator` through the sim's
`run_autonomous_slot` → real `QsoManager`.

**Production coordinator wiring: NOT YET (deliberate plan-sized follow-on).**
Today no production path creates an `Auto` QSO in the `QsoManager` — the
autonomous task (`coordinator/autonomous.rs`) only emits gated
`TransmitRequest`s; the universal decode→`process_message` loop
(`coordinator/qso.rs:966`) therefore never has an Auto QSO to advance. To
enable autonomous completion on-air, the autonomous task must register each
surviving opening (pounce → `respond_to_cq` Auto; CQ → `start_cq` Auto) in the
`QsoManager` so the engine drives it. Open design points (resolve before
touching the live TX path):
  1. **Frequency-model split.** The autonomous operator smart-allocates a TX
     offset that differs from the DX's decode frequency, but `QsoState` carries
     ONE frequency (manual answers *on* the DX freq). The QSO needs an
     RX-match-freq (DX, for `is_message_relevant`) distinct from the TX-freq
     (our offset). Until split, an autonomous QSO would either mismatch the
     DX's frames or TX on top of the DX. (In the sim today we sidestep this by
     answering on the DX freq — fine for the tests, wrong for production.)
  2. **Double-send avoidance.** The autonomous task already sends the gated
     opening `TransmitRequest` (qso_id=None). If `respond_to_cq` also emits its
     opening `MessageToSend` (→ forwarded at `qso.rs:718`), the opening goes
     out twice. Need a register-only QSO-creation that emits `StateChanged`
     (to populate `active_tx_qsos`) but NOT the opening `MessageToSend`.
  3. **Gating order.** QSO creation must happen only for openings that survive
     the Shift+Q runtime gate and the tri-state TX policy (both applied to
     `tx_items` AFTER the action loop). Create QSOs from surviving items, not
     inside the action loop.
  4. **Coordinator integration test.** Add a `coord_sim`-pattern test that
     drives an autonomous pounce end-to-end through the coordinator to a logged
     completion, asserting exactly one opening TX and no double-send.
  5. On-air A/B validation is operator-gated (needs antenna) — meatspace-pending.

## Conventions
Each scenario → a named test with a slot-by-slot exchange + asserted outcome, citing source
(LOG / PEER / IDEA) + the original incident where applicable. A scenario that reveals a real
bug (frustration would still happen) is committed as `#[ignore]` with `// KNOWN BUG:` + a note,
then fixed in a coordinated follow-up and un-ignored. Status tracked here.
