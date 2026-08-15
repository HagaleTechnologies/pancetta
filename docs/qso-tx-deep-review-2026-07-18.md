# QSO management + TX physical-path deep review — 2026-07-18

> **Status: remediation COMPLETE (2026-07-18).** All 5 batches in the executive
> synthesis's remediation order landed: Batch 1 `6b2ceaf9` (double-PTT closure), Batch 2
> `584dd81a` (responder parity latch + bundle verification), Batch 3 `a43419df`
> (frequency-allocator correctness), Batch 4 `e16b0370` (completion-rate fixes), Batch 5
> `6927e02c`/`41a8d8da` (second tier: Hold-mode offset, auto-73 guard, protocol-aware
> pivot, multi-TX defer, parity-aware scoring, live instrument when disabled). Every fix
> was independently re-verified by the controlling session (diff review + full test
> suite + clippy) beyond the implementing subagent's own report before landing. Doc
> reconciliation (this file's own findings against `docs/DECISIONS/{qso-engine,
> tx-scheduling}.md`, the 2026-04-27/2026-04-29 specs, and `docs/qso-state-machine-
> analysis.md`) is also complete. Deferred items (TX-F3/TX-F7 worker concurrency, SM-F7/
> F9/F10/F11/F12, FQ-F11/F12/F13, FQ-F10 stuck-hop stream-blindness) remain open per the
> "Out of scope / deferred" section below — none were silently dropped, they were never
> in the 5-batch plan. The analysis below is preserved as-written (the historical
> record); trust the code + the DECISIONS docs' newer notes for current behavior where
> they've since been updated.

A three-track deep review of (A) the QSO state machine and progression logic,
(B) automatic TX audio-offset selection, and (C) TX scheduling / parity /
multi-TX. Conducted against `origin/main @ 3bd81e86` (worktree
`dx-hunter-entity-atno-perband`). Successor to `docs/qso-state-machine-analysis.md`
(2026-07-03), which is now (fully) superseded (see §A.4).

Findings are prefixed **SM-** (state machine), **FQ-** (frequency selection),
**TX-** (scheduling/multi-TX). CONFIRMED = code path traced fully; SUSPECTED =
mechanism verified, occurrence not reproduced. Load-bearing claims (FQ-F1,
FQ-F2, FQ-F3, FQ-F7, SM-F2, SM-F5, TX-F1 pivot mechanism) were independently
re-verified by the controlling session against the source.

---

## Executive synthesis

1. **The open double-PTT-for-73 bug has a coherent root-cause theory, reached
   independently by both the state-machine track and the TX-scheduling track:
   the Step-4c late pivot duplicates the 73.** The pivot (`tx.rs:1708-1770`)
   rewrites an already-dequeued, in-flight frame (typically a keep-call rearm
   rung waiting out the pre-PTT sleep) to the freshest `latest_tx_intent` text —
   but nothing consumes the intent, and the *newer* `TransmitRequest` carrying
   that same 73 is still in the channel. Both frames pass the Step-4b liveness
   gate because the 45 s completed-TX grace (the G8KHF fix) keeps the qso_id
   live. Result: one `MessageToSend(73)` emission (matching the PR #158 trace),
   two dequeues with **different msg ids and different original texts**
   (explaining why PR #159's same-id / same-content searches found nothing),
   two PTTs with the identical 73 — 30 s apart with latched parity, 15 s apart
   when parity is unlatched (SM-F2/TX-F2). The pivot's only log line uses
   `target: "tx.pivot"`, which the default `EnvFilter` (`pancetta=...`) drops —
   so all tracing to date was structurally blind to the very mechanism.
   **Smoking gun for the next live occurrence: the "TX pivot:" line.**
   A secondary, bounded double-73 producer also exists (SM-F3/TX-F10:
   `maybe_auto_resend_73` can fire while the original 73 is still deferred) but
   is distinguishable — it emits a second `MessageToSend`, which #158 would
   have caught.

2. **The half-duplex same-parity invariant is not actually enforced.** Responder
   paths (`respond_to_cq_with:1226`, `respond_to_caller:1469`) store
   `dx_parity.map(opposite)` — `None` stays `None` forever. The BUG-1 latch
   (`latch_cq_parity_if_none`) was applied only to the CQ paths. A QSO opened
   from a parity-less source (DX-cluster/DX-Hunter spot) re-resolves "nearest
   next slot" per emission and **alternates TX windows** — the exact BUG-1
   failure, going deaf to replies and defeating admission gating
   (`current_tx_side()` sees it as side-less). The 2026-04-27 spec promised
   "the first received decode will refine the parity"; that refinement was
   never implemented. Downstream, neither the coalescer (`tx.rs:979`) nor the
   autonomous bundler verifies bundle parity — the first item's parity is
   coerced onto the bundle with at most a warning.

3. **Automatic TX frequency selection is compromised by several independent,
   confirmed bugs that compound**: the spectral axis is misaligned ~100–200 Hz
   (bins start at 0 Hz, labeled 200 Hz), per-slot occupancy is stamped with
   wall-clock parity at feed time (frequently inverted), the own-frequency
   registry is never populated in production, the busy-bin scoring term is
   floored at +15 by an inverted `max`, and scoring is parity-blind. The
   CLAUDE.md "single-scorer" invariant holds only narrowly (autonomous CQ in
   Auto mode vs display); the default Hold mode pins the *config* offset — the
   operator's parked offset never reaches the autonomous engine — and the
   pounce path discards the allocator's pick entirely (Tx=Rx).

4. **Autonomous concurrent streams have no minimum-separation enforcement.**
   Only the manual path de-conflicts (75 Hz). Two DXes CQing within ~75 Hz can
   produce overlapping own streams; worse, `modulate_multi_tx` then fails the
   *whole bundle* (pairwise separation check), so two individually-fine streams
   can livelock failing every slot until the watchdog retires them (TX-F6).

5. **The state-machine ladder itself is fundamentally sound** — every rung has
   transition + reply arms, sender verification everywhere, compound-callsign
   handling, early closes, prior GAP-1/GAP-2/Symptom-A/B fixes all present.
   The residual weaknesses are completion-rate holes: the CQer side has **no
   dropped-report resilience** (`WaitingForReport` + repeated grid = silent
   death in 30 s — hits manual CQ, autonomous CQ, and Fox), Auto QSOs still
   have no re-send/regression (GAP-3 half-fixed), and `QsoEvent::QsoFailed` is
   never emitted so priority-scoring failure backoff is dead code.

6. **Cross-cutting architectural observation**: "exactly one TX per slot per
   QSO" rests on a lattice of timing stamps (`last_call_at`, per-slot dedup,
   800 ms coalesce window, 45 s grace, pivot) rather than any positive
   invariant such as "at most one in-flight frame per (qso_id, text)" or a
   worker-level recent-TX dedup. Every double-TX finding in this review is a
   crack in that lattice. The single highest-leverage structural fix is a
   worker-level (qso_id, text, slot) consume-once/dedup discipline.

### Consolidated remediation order (proposed, not started)

1. **Double-PTT closure** — pivot consume-once/tombstone of `latest_tx_intent`
   + worker-level (qso_id, text) recent-TX dedup; re-prefix `tx.pivot` (and
   audit sibling targets) so the pivot is visible at default log level.
   (TX-F1/SM-F1; log-target audit already flagged in DECISIONS.)
2. **Parity latch on responder paths** (+ first-decode refinement per spec) and
   bundle-parity verification at the coalesce choke point. (SM-F2/TX-F2, TX-F5.)
3. **Frequency-allocator correctness batch** — spectral axis label (FQ-F1),
   decode-parity stamping (FQ-F2), scoring floor (FQ-F7), own-frequency
   registration + autonomous-path deconfliction (FQ-F3/FQ-F4), separation
   pre-check before bundle fold (TX-F6).
4. **Completion-rate batch** — CQer `WaitingForReport` re-send arm + rearm
   coverage (SM-F4), bounded Auto re-send/regression (SM-F6), emit `QsoFailed`
   at the three terminal sites (SM-F5).
5. **Second tier** — Hold-mode parked-offset wiring (FQ-F6), parity-aware
   scoring (FQ-F8), auto-73 pending-original check (SM-F3), protocol-aware
   pivot re-encode (TX-F4), multi-arm defer handling (TX-F8), instrument alive
   when autonomous disabled (FQ-F9).
6. **Docs** — fix the stale/contradictory statements catalogued in §A.4, §B.4,
   §C.4 (including CLAUDE.md's single-scorer and same-parity invariants, which
   are currently aspirational).

---

# Track A — QSO state machine & progression

Files: `pancetta-qso/src/qso_manager.rs` (8,827 ln, read fully), `states.rs`,
`exchange.rs`, `pancetta/src/coordinator/qso.rs` (5,134 ln, read fully), plus
`coordinator/tx.rs` (schedule/coalesce/pivot), `coordinator/mod.rs`
(`tx_pivot_target`), `async_database.rs`, `async_logger.rs`.

## A.1 State machine map

### States (`states.rs:25-104`)
`Idle`, `CallingCq`, `RespondingToCq`, `WaitingForReport`, `SendingReport`,
`WaitingForConfirmation`, `SendingConfirmation`, `Completed`, `Failed`,
`Contest(ExchangingInfo|ContestCompleted)`.

- **`SendingConfirmation` is DEAD** — never constructed in production (only a
  states.rs test). Still carries live surface: `ladder_rank`
  (qso_manager.rs:2257), repetitive-TX watchdog list (:3773), coordinator AP
  mapping (coordinator/qso.rs:1529), ladder view, snapshot label. Prior GAP-4.
- **`Contest(...)` unreachable** — coordinator builds `QsoManagerConfig` with
  `..Default::default()` (qso.rs:1103-1127) so `contest_mode: None`; no arm
  produces Contest states; `generate_contest_response` (exchange.rs:822) gated
  on `contest_mode.is_some()`.
- **`Idle`** only a synthetic `old_state` for birth events (:990, :1312, :1606).
- **Dead events**: `QsoEvent::QsoFailed` and `QsoEvent::DuplicateDetected`
  (:362, :369) never emitted by production `QsoManager` (only sim.rs) — SM-F5.
- **Dead helpers**: `exchange.rs::validate_exchange_sequence` +
  `QsoSequenceState` (:901-960), `is_duplicate_message` (:622),
  `AutoSequenceConfig` (qso_manager.rs:295-310, no field ever read).

### QSO creation entry points (qso_manager.rs)
| Entry | Line | Initial state | Role/Init | tx_parity latch |
|---|---|---|---|---|
| `start_cq` | 777 | CallingCq | Cqer/Auto | latched via `latch_cq_parity_if_none` (751) |
| `start_cq_manual` | 898 | CallingCq | Cqer/Manual | latched (917) |
| `respond_to_cq(_manual)` → `respond_to_cq_with` | 1156 | RespondingToCq | Caller/Auto or Manual | `dx_parity.map(opposite)` — **`None` stays `None`** (1226) |
| `engage_hound` | 1083 | RespondingToCq (+hound, partner_freq) | Caller/Manual | None stays None |
| `respond_to_caller` (Report/ReportAck/Rr73/73) | 1367 | SendingReport / WaitingForConfirmation / Completed | Caller/Manual | `None` stays `None` (1469) |
| `advance_existing_qso_to_step` | 3252 | jumps existing manual QSO | preserved | preserved |

Creation guards: placeholder-callsign refusal; Auto-only `check_duplicate`
(1176, 3116); Manual FIX-1 continue-existing (`find_active_manual_qso_for`,
3209 — re-emits `resend_last_tx`, returns existing id); FIX-3 supersede
same-call/same-band (3409 → `Failed{Superseded}`, mapping removed).

### Transitions (`determine_state_transition`, qso_manager.rs:2263-2902)
Every advancing arm verifies sender (`is_partner` via `callsigns_match`, and
`is_us(to)`); mismatch ⇒ `warn!(target:"qso.security")` + no change.

Forward arms:
1. `CallingCq + CqResponse(to us)` → `WaitingForReport` (latch caller+grid) — 2278.
2. `CallingCq + SignalReport(to us, any from)` → `WaitingForReport` (A4 skip-grid) — 2317.
3. `WaitingForReport + RR73|73` → `Completed` (A5 early close, their_report defaulted −15) — 2354.
4. `WaitingForReport + ReportAck` → `WaitingForConfirmation` — 2401.
5. `RespondingToCq + CqResponse(us↔DX)` → `SendingReport{their_report:None}` (stuck-at-grid) — 2456.
6. `RespondingToCq + SignalReport` → `SendingReport{their_report:Some}` — 2497.
7. `RespondingToCq + ReportAck` → `WaitingForConfirmation` (skip-rung) — 2540.
8. `RespondingToCq + RR73|73` → `Completed` (GAP-1 arm, present) — 2586.
9. `SendingReport + ReportAck` → `WaitingForConfirmation` — 2626.
10. `SendingReport + RR73|73` → `Completed` (FIX-2 early close) — 2667.
11. `WaitingForConfirmation + RR73|73` → `Completed` — 2707.

Regression arms (**gated `initiated_by == Manual`**):
- R1 `WaitingForConfirmation + SignalReport` → `SendingReport` (re-emits R) — 2761.
- R2 `SendingReport + SignalReport` → stays (their_report refreshed; re-send owned by rearm) — 2816.
- R3 `WaitingForConfirmation + CqResponse` → `RespondingToCq` — 2859.

Reply emission (`process_message_for_qso`, 1801-2224): fired iff
`new_state != old_state || is_manual_regression` (1928); reply generated from
the *pre-transition* `(state, message)` pair via `exchange.rs::generate_response`
(417-601), recorded as Sent, emitted after lock drop. Forward auto-reply applies
to **both** Manual and Auto (Phase 5); regression replies Manual-only.

### Timeouts / watchdogs (`check_timeouts_at`, 3753-3899; 5 s ticker at 3548)
- **Repetitive-TX watchdog** (first, all QSOs, all 6 active states incl. dead
  `SendingConfirmation`): same state > `repetitive_tx_timeout_secs` (300 s) ⇒ Timeout.
- **Manual keep-call watchdog** (Manual + CallingCq/RespondingToCq/SendingReport):
  retire at 5 min OR 25 calls; call-cap only on initial-call states (9K2MP fix
  :3831); `progressed_this_cycle` one-pass reprieve (C3, :3816). These states
  skip per-state timeouts.
- **Per-state timeouts**: CallingCq→30 s, WaitingForReport→30 s,
  WaitingForConfirmation→30 s, Auto RespondingToCq/SendingReport→30 s.
- Retirement: **`qsos.remove()`** + `StateChanged→Failed{Timeout}` + mapping
  removal — record *deleted*, not kept as Failed (asymmetric with supersede;
  `cancel_qso` :1700 also removes).
- **Keep-call rearm** (`rearm_manual_calls_at`, 3579): Manual only; per state
  re-emits `Cq`/`CqResponse`/`SignalReport`(their_report None)/`ReportAck`
  (their_report Some); ~1/slot via `last_call_at`; bounded by call cap; forward
  advance and regression both stamp `last_call_at` (2042, 2074) — Symptom-A fix
  present. Cadence hardcoded 15 s (TX-F11: not protocol-scaled).
- Coordinator-side: pending cross-parity TTL 10 min (qso.rs:149, 519); auto-73
  window 3 min / 3 resends / 14 s spacing (:37-43); completed-TX grace 45 s (:1776).

### Coordinator ↔ manager split (coordinator/qso.rs)
- Decode loop (2074-2213): parse → `maybe_auto_resend_73` (2096) →
  `process_message` (2112) → per-slot creation dedup (2124-2177) →
  `maybe_answer_caller` (#39, autonomous-independent, 845) → DX-activity record.
- Command handlers: StartQso (2218, #40 cross-parity queue + T3
  held-offset/de-conflict), StartAutonomousQso (2393), EngageHound (2478),
  RespondToCaller (2607), Abort/End/Resend/CancelAll/BandChanged/StartCq/StopCq/
  SetFoxMode/SetOperatingMode.
- Event-forwarding task (1438-2049): `StateChanged` → maintain `active_tx_qsos`
  + `latest_tx_intent` + AP context + snapshot; `MessageToSend` → render text,
  **insert `latest_tx_intent`** (1701), forward one `TransmitRequest`;
  `QsoCompleted` → 45 s grace insert + delayed purge + promote pending;
  `QsoFailed` → purge + `record_failure` (dead — SM-F5).
- Manager is the single source of TX content; coordinator is the single
  MessageToSend→TransmitRequest converter (verified: WSJT-X UDP subscription
  does not forward TX).

## A.2 Progression assessment

- **Out-of-order / skipped rungs**: well covered on the Caller ladder (A4,
  skip-rung, FIX-2, GAP-1) and partially on the CQer ladder. Missing: CQer
  two-rung skip (`CallingCq + ReportAck`) — no-op (prior GAP-6, open).
- **Repeated frames / missed acks**: Manual — solid (R1/R2/R3 + rearm +
  last_call_at coordination). **Auto — no re-send and no regression at all**
  (rearm Manual-only :3598; regression arms Manual-gated) — SM-F6.
- **CQer-side missed ack — structural hole** (SM-F4): in `WaitingForReport` a
  caller who missed our report repeats their grid (`CqResponse`) — no arm, no
  reply, rearm excludes the state, manual watchdog excludes it too → dies at
  30 s `report_timeout` even for Manual. `maybe_answer_caller` can't rescue
  (suppressed by `has_active_or_recent_qso_with`). Affects manual `c` CQ,
  autonomous CQ, and Fox mode.
- **RR73 vs RRR**: both parse to `FinalConfirmation` (exchange.rs:730-741;
  RR73-not-a-grid ordering fix present); handled identically everywhere.
- **73**: RR73 ⇒ complete + reply 73; bare 73 ⇒ complete + no reply (QRM
  avoidance) — consistent. Post-completion repeated RR73 → bounded auto-73
  (but see SM-F3).
- **Early close**: covered from RespondingToCq/WaitingForReport/SendingReport.
- **Compound callsigns**: `callsigns_match` in every partner/us check;
  logged-callsign upgrade gated by `is_safe_compound_upgrade` (exchange.rs:169).
  Sound.
- **Simultaneous/duplicates**: FIX-1 idempotent re-call, FIX-3 supersede,
  answer-caller gating, per-slot dedup. Residual: SM-F7 fan-out below.

## A.3 Findings

**SM-F1 — Step-4c pivot duplicates one message across two TransmitRequests
(double-PTT prime candidate). SUSPECTED (mechanism CONFIRMED; occurrence not
reproduced). HIGH.**
`tx.rs:1708-1770`, `mod.rs:118-130`, `qso.rs:1701`, grace `qso.rs:1776`.
Every `MessageToSend` writes `latest_tx_intent[qso_id]`. If an older frame for
the same QSO is in the TX worker's pre-PTT wait (up to ~30 s), Step 4c rewrites
its text to the newest intent and transmits it — but the newer message's own
`TransmitRequest` remains queued (the coalescer at tx.rs:844 dedups only
backlog present at pickup) and there is no consume/tombstone of the intent nor
any "(qso_id, text) already sent" suppression. Both pass Step 4b via the 45 s
grace. Concrete 73 scenario: Manual `SendingReport`; 5 s ticker rearm emits
`ReportAck` (frame A) just before the DX's RR73 decodes; Completed emits the 73
(frame B) → intent = "…73"; frame A pivots to "…73" and keys; frame B pops
after A's TX and keys "…73" again next resolved slot. Identical message +
qso_id, two PTTs — the traced signature. Spacing 30 s latched, **15 s
unlatched (SM-F2)**. Aggravator: `target: "tx.pivot"` (tx.rs:1752) is invisible
at the default EnvFilter — tracing to date never saw a pivot. Predicted #158/
#159 signature: ONE emit for the 73, a dequeue of a different text plus a
dequeue of the 73, two PTT-ONs same text.

**SM-F2 — Responder-path QSOs never latch `tx_parity` when `dx_parity == None`.
CONFIRMED. HIGH.**
`qso_manager.rs:1226`, `:1469` vs latch only at 751/796/917. With `None` and
`TxSelfParity::Auto` the worker re-resolves nearest-next-slot per request
(tx.rs:2894-2917) — the BUG-1 mechanism. Consequences: (a) keep-calling manual
QSO from a parity-less source (DX-cluster/Hunter spot — doc at 1020-1022 names
the case) TXes on alternating windows, deaf to replies, violating the CLAUDE.md
invariant; (b) any duplicated frame lands 15 s apart — matching observed
double-PTT spacing; (c) side-less in `current_tx_side()` (534) →
`admit_new_qso` can admit a genuinely conflicting QSO. Fix shape: latch in
`respond_to_cq_with`/`respond_to_caller` (or refine from first partner decode
per the 2026-04-27 spec §"parity refinement").

**SM-F3 — `maybe_auto_resend_73` can fire while the original 73 has not yet
transmitted → two 73s (different qso_ids). CONFIRMED. HIGH-MED.**
`qso.rs:583-719, 2096`. Gate checks only: stashed manual completion <3 min,
resends <3, ≥14 s since its own last resend, TxPolicy, no *active* QSO with
sender. The just-completed QSO is Completed (not active), so: (a) DX repeats
RR73 before our deferred original 73 keys → second Completed QSO + second 73 in
consecutive same-parity slots; (b) same-window duplicate decodes of one RR73
(decoders emit 2-4 copies; auto-resend runs per copy *before* `process_message`)
race the event task → immediate extra 73 (`last_resend_at: None` first time).
No check against `latest_tx_intent`/pending TX. Distinguishable from the traced
same-qso_id bug; cf. ZL1UHD "four 73s" history.

**SM-F4 — CQer role has no dropped-report resilience. CONFIRMED. MED-HIGH.**
No `(WaitingForReport, CqResponse)` transition or reply arm (2263-2902,
exchange.rs:417-601); rearm excludes `WaitingForReport` (3601-3652); manual
watchdog list excludes it (3794-3799) → 30 s `report_timeout` (3847). The
single most common FT8 retry (caller re-sends grid) gets silence and the QSO is
retired in two slots. Impacts manual `c`, Fox (Hound lost after one missed
report), autonomous CQ. Fix: CQer-side regression/re-send arm mirroring R2 +
rearm coverage for `WaitingForReport` (re-send our `SignalReport`).

**SM-F5 — `QsoEvent::QsoFailed` never emitted ⇒ failure backoff dead.
CONFIRMED. MED.**
Producers `cancel_qso` (1700), `check_timeouts_at` (3878-3898),
`supersede_active_qsos_for` (3432-3459) emit only `StateChanged`; grep confirms
no production emission (only sim.rs). Consumer `qso.rs:1946-2038` incl.
`qso_lookup.record_failure` (2035) — the recency/backoff penalty in priority
scoring — never runs. Autonomous protection against hammering a non-answering
station rests solely on dup gate + `dx_busy_window`. The failure Warn
DiagnosticEvent (2016-2028) also never fires. Fix: emit `QsoFailed` alongside
terminal `StateChanged` at the three sites.

**SM-F6 — Auto QSOs still have no re-send/regression (GAP-3 residual).
CONFIRMED. MED.**
Rearm gate :3598; regression arms gated 2773/2829/2870. An autonomous pounce
whose grid or R-report the DX misses is silently retired at 30 s while the DX
actively repeats. Phase-5 extended only forward replies to Auto (1917-1928).
Largest completion-rate loss for live autonomous operation.

**SM-F7 — Message routing can fan one frame into multiple QSOs / pollute logs.
CONFIRMED. MED. Partially FIXED 2026-08-14 (PAN-14).**
`find_qsos_for_message` (2904) loops all active QSOs; default relevance arm
`_ => message_type.is_addressed_to(our_callsign)` (3078-3081) routes any to-us
frame into any active QSO lacking an explicit arm, gated only by the 100 Hz
window. Effects: (a) foreign frames in the wrong QSO's `messages`; (b) the
stuck-DX detector (2100-2130) counts them — four repeats of a third station's
frame within 100 Hz can trigger a spurious offset hop in `TxFreqMode::Auto`;
(c) a `CallingCq` QSO + existing same-station QSO can both accept one frame
(A4 accepts any from_station at 2317-2340) → second parallel QSO with the same
partner that supersede never sees (runs only at creation). Fix: require
`is_partner` in the default arm when partner latched; exclude stations with an
existing active QSO from the CallingCq accept.

**PAN-14 update (2026-08-14):** effect (c) was root-caused as the mechanism
behind an on-air double-TX report (two high-amplitude signals for one station
in one window, 2026-08-11) and reproduced deterministically in
`qso_manager::tests::calling_cq_and_established_qso_both_accept_same_partners_frame`
(`pancetta-qso/src/qso_manager.rs`): a `CallingCq` QSO's two "any station"
relevance arms (`CqResponse`/bare `SignalReport` addressed to us) now also
require the sender have no OTHER established-or-recent active QSO
(`sender_has_other_active_or_recent_partner`, computed once per incoming
message in `find_qsos_for_message` from `metadata.their_callsign` across all
active QSOs, compound-callsign-aware via `callsigns_match`). Effects (a)/(b)
— the fully general default-arm case for state/message-type combinations with
no explicit arm — are UNFIXED; still open, narrower in practice than (c)
since most CallingCq/established-state × message-type pairs already have
explicit arms.

**PAN-14 round-1 review update (2026-08-15):** Codex's first review of PR
#250 found two adjacent gaps in the same guard, both fixed and covered by
regression tests in the same commit:
- **P1 — two unpartnered `CallingCq` QSOs.** The guard above is keyed on an
  ESTABLISHED partner (`metadata.their_callsign` latched); it has no
  visibility into a conflict between two still-`CallingCq` QSOs (both
  `their_callsign == None`) — e.g. from repeated `StartCq`/`start_cq_manual`
  calls or Fox mode engaging while a CQ is already live (neither path
  supersedes an existing `CallingCq` QSO). A single reply could independently
  satisfy both QSOs' "any station" arms. Fixed in `find_qsos_for_message`:
  when more than one `CallingCq` QSO survives relevance matching for the same
  message, only the earliest-created (`metadata.start_time`) is kept — first
  CQ up, first CQ answered. Test:
  `one_reply_can_only_advance_the_earliest_calling_cq_not_multiple`.
- **P2 — recently-completed QSOs were invisible to the guard.** The original
  guard's predicate was `p.state.is_active()` only, so a QSO that just
  `Completed` dropped out immediately — a stray/duplicate frame from that
  exact station could be re-claimed by an unrelated `CallingCq` QSO. Fixed by
  extending the guard to also count a `Completed` QSO with the same partner
  within `COMPLETED_QSO_REWORK_GRACE` (45 s), mirroring
  `has_active_or_recent_qso_with`'s existing active-or-recent pattern. Test:
  `recently_completed_qso_reserves_the_sender_from_an_unrelated_calling_cq`.

**SM-F8 — Re-send paths bypass the rearm's anti-double-send stamps. CONFIRMED.
MED.**
`resend_last_tx` (1756-1777) stamps neither `last_call_at` nor `call_count`;
callers: operator `ResendQso`, FIX-1 keep-call (1208), `respond_to_caller`
re-send (1455). Space-mashing near a slot boundary → same-text emissions
>800 ms apart → coalescer misses, pivot no-ops (identical text), each surplus
request transmits in a later slot (same text+qso_id — another double-PTT
shape). Also uncounted against the manual cap.

**SM-F9 — Duplicate-check frequency semantics unit-mismatched; DB dup detection
vacuous. CONFIRMED. MED.**
`check_duplicate` (3116-3184): in-memory compares audio offsets (±50 Hz) — a
station re-CQing at a different offset within 24 h passes the Auto gate. DB:
logger persists RF (dial+offset) (async_logger.rs:731-756; dial stamped
qso_manager.rs:1995-2010) while the query passes audio offset —
`ABS(diff) < 50` (async_database.rs:655-669) never matches with
`check_frequency = true` (default 418-427) → cross-restart dup detection never
fires. Related: "same band" in supersede/FIX-1 (`frequency_to_band(audio_offset)`,
3210/3410) collapses all offsets to one pseudo-band — self-acknowledged, works
today, breaks silently if per-QSO RF is threaded.

**SM-F10 — Terminal bookkeeping relies on a lossy broadcast; lagged terminal
`StateChanged` permanently leaks `active_tx_qsos`. SUSPECTED (low prob). MED
impact.**
Set maintained only from the event stream (qso.rs:1456-1487); channel cap 1000;
`Lagged` logged (2041-2043) without resync. A dropped terminal event leaves the
id in `active_tx_qsos`/`latest_tx_intent` forever — Step 4b passes stale frames
indefinitely. No periodic reconciliation vs `get_active_qsos()`. Same class:
final-73 depends on the `QsoCompleted` grace insert (1789) racing ahead of the
worker's pickup filter — comment (1598-1605) asserts ordering within the serial
event task, but the worker is concurrent; a pickup microseconds before the
grace insert drops the 73.

**SM-F11 — Inconsistent terminal retention. CONFIRMED. LOW.**
Watchdog/cancel `remove()` (3879, 1702) — no Failed record; supersede mutates
in place. `has_active_or_recent_qso_with`'s "recent" only ever sees Completed.

**SM-F12 — Dead/vestigial surface. CONFIRMED. LOW.**
`SendingConfirmation`, `QsoEvent::DuplicateDetected`, `AutoSequenceConfig`,
`validate_exchange_sequence`/`is_duplicate_message`,
`DuplicateCheckConfig.check_band` (unread; qso.rs:1120-1123 acknowledges),
`MessageType::ContestExchange` (no arm). Also qso.rs:1734-1744 in-code BUG
comment: encode failure of a `MessageToSend` leaves the QSO waiting for a TX
that never happened, no `QsoFailed` (compounds SM-F5).

**SM-F13 — Known-open cross-checks.**
Symptom C: structure unchanged (800 ms first-pickup window can't batch serial
keypresses) — consistent with deferred status. Double-PTT: the merged
`emit_event` diagnostic (qso_manager.rs:3474-3511) will show one emit for the
73; SM-F1+SM-F2 predict duplication downstream. Recommend worker-level
(qso_id, text, slot) dedup + log-target re-prefix.

## A.4 Doc inconsistencies (state machine)

1. `docs/DECISIONS/qso-engine.md` §"Manual CQ": says manual CQ `tx_parity` is
   `None` resolved by fallback — **stale**: `start_cq_manual` latches at
   creation (917; test 5601). The actual unlatched-`None` case lives on the
   responder paths (SM-F2), unmentioned.
2. Same doc, watchdog cap contradiction: §"Manual vs automated" 5 min/25
   (correct, matches `TimeoutConfig::default` 399-401); §"Manual CQ" "5 min/10"
   (stale).
3. §"Sender verification": "tolerance is 15 Hz" — stale: 15 Hz only for
   initial/ambiguous matching; established QSOs use 100 Hz (B15, 2958); Hound
   matches `partner_freq` (3101-3111).
4. §"Manual vs automated": "re-emits one CqResponse per slot" — rearm now
   re-emits Cq/CqResponse/SignalReport/ReportAck by state (3601-3652).
5. `docs/qso-state-machine-analysis.md` should be annotated: GAP-1, GAP-2,
   Symptom A, 9K2MP scoping FIXED; GAP-3 HALF-fixed (SM-F6); GAP-4, GAP-5 (no
   self-echo reject — still nothing filters `from == our_callsign` before
   `process_message`), GAP-6 open. Its "MultiTransmitRequest arm has no pivot"
   claim is stale (multi arm re-encodes at key-time, tx.rs:2091, 2490).
6. CLAUDE.md same-parity invariant violated by SM-F2 and transiently by
   SM-F1/SM-F3 duplicates during the 45 s grace.
7. End-to-end spec: phase plan, not behavioral contract; its "Concurrent QSO
   manager: `pancetta/src/qso_manager.rs`" location never materialized.

---

# Track B — Automatic TX frequency (offset) selection

Files: `pancetta-qso/src/frequency.rs` (all), `pancetta-qso/src/autonomous.rs`
(allocator paths), `pancetta/src/coordinator/autonomous.rs`,
`coordinator/qso.rs` (manual offset), `coordinator/tui_relay.rs`,
`pancetta-ft8/src/decoder.rs` (waterfall gen), `pancetta/src/cqdx_bridge.rs`.

## B.1 Mechanism map

### Data in (occupancy picture)
- **Decodes**: FT8 decoder fans decodes to Autonomous (ft8.rs:1288-1302) with
  correct per-window `slot_parity` (ft8.rs:79-86, applied 1098-1101). The slot
  loop accumulates (coordinator/autonomous.rs:1188-1218) and feeds per 15 s
  tick via `feed_decoded_messages_at` (pancetta-qso/autonomous.rs:886-927) →
  `DecodeRecord { frequency_hz, time_slot }` — **`time_slot` derived from
  `SlotParity::current()` at feed time, ignoring `msg.slot_parity`**
  (:914-927 — FQ-F2). Records → `DecodeHistory` (rolling 4-cycle ≈ 60 s;
  frequency.rs:119-163); `push_cycle` every tick, so eviction time-based.
- **Spectrum**: one waterfall per decode window covering **0–3000 Hz**
  (decoder.rs:8214-8241, `bin_start = 0`), rows min-max normalized per window
  (ft8.rs:717-731), `try_send` over bounded(2) channel (mod.rs:1192-1193). The
  tick `try_recv`s one batch, averages, wraps as
  `SpectralSnapshot { freq_min_hz: 200.0, freq_max_hz: 3000.0 }`
  (coordinator/autonomous.rs:628-647) — label does not match data (FQ-F1).
- **Live spots**: `cqdx_bridge.spot_frequencies()` → `update_live_spots`
  (:649-652) — absolute RF Hz (cqdx_bridge.rs:372-384; `SpotGroup.frequency:
  u64`, pancetta-cqdx/types.rs:88).

### Scoring and pick
`SmartFrequencyAllocator::rank_candidates` (frequency.rs:221-259): sweep
200→2800 in 25 Hz steps; `score_candidate` (:261-325) sums 7 soft criteria:
clear-both-slots +30 (else floored partial credit — FQ-F7), noise floor 0–20,
neighbor peak within 100 Hz 0–15, recent activity 0–10, center bias 0–10, DX
proximity 0–8 (sweet spot 50–200 Hz from DX), own-frequency separation −50
inside 75 Hz. Per-parity `clear_first`/`clear_second` flags computed but
**consumed by no decision path** (display only; tui_relay.rs:585-586).

`allocate_smart_frequency(dx_target)` (autonomous.rs:1063-1097):
1. **Hold mode (default)** → `config.tx_offset_hz` (static config value, not
   the operator's parked offset — FQ-F6).
2. Auto → `rank_candidates`; CQ (dx=None) applies
   `apply_live_spot_rarity_boost` (:1042-1059, +0.2·rarity within 200 Hz of a
   spot — dead, FQ-F5); take first.
3. No spectral snapshot yet → legacy `allocate_cq_frequency` (:681-716).

### Latch / hold
- `decide()` (autonomous.rs:1254-1542): pounce → allocator with Some(dx freq)
  (:1462); self-CQ → allocator None (:1496); emits `OperatorAction::Transmit`.
- Coordinator: `plan_slot_transmissions` (coordinator/autonomous.rs:252-324) →
  `classify_autonomous_opening` (:33-59): **pounce discards the allocator pick,
  substitutes Tx=Rx on the DX's decoded frequency**; only self-CQ keeps the
  pick. → `StartAutonomousQso` (qso.rs:2393-2477) → `respond_to_cq`/`start_cq`
  latch into `QsoMetadata.frequency` once; every reply reads it — offset holds
  for the exchange.
- **Manual**: `compute_manual_tx_offset(dx_freq, hold_mode, held_hz,
  active_offsets)` (qso.rs:197-220; sites :2333, :2628, promotion :436-464):
  held (Hold) else Tx=Rx → `deconflict_offset` vs `active_tx_offsets()`
  (qso_manager.rs:127-146; 75 Hz min sep, 25 Hz outward, clamp 300–2700) →
  `partner_freq = Some(dx_freq)` when diverged so relevance still routes.
- **Hound**: hash-to-region `hound_offset_for` (qso_manager.rs:98-113), QSY on
  Fox report (:2148-2180).

### Escape hop (only mid-QSO mover)
`process_message_for_qso` (qso_manager.rs:2083-2130): consecutive identical
non-advancing DX frames; at `DX_STUCK_REPEAT_THRESHOLD = 4` and only
`TxFreqMode::Auto`, hop `stuck_hopped_offset` (+300 wrap [300,2700], :84-93),
mutating `metadata.frequency` + in-flight `qso_frequency`, reset streak. Blind:
no spectrum (documented deliberate), no other-stream deconfliction (not
documented — FQ-F10). Relevance survives because the *state's* embedded
frequency is deliberately not updated (DX frames still match pre-hop within
100 Hz) — correct today, fragile undocumented coupling (contrast Hound QSY,
which does update the state :2164-2172).

Other movers, all gated off live QSOs: collision-listen jitter
(autonomous.rs:1544-1589, Auto + `active_qso_count == 0` only, ±200 Hz,
reseeds next opening); auto-repark (coordinator/autonomous.rs:114-168, 744-830,
opt-in, fail-closed, no-QSO gate re-checked without `.await` before write —
sound).

### Multi-stream
- Separation enforced **only on the manual path** (`deconflict_offset`) —
  FQ-F3/FQ-F4.
- Bundling: `MultiTransmitRequest` sums per-item waveforms at offsets relative
  to base 200 Hz, 0.5 amplitude scaling (tx.rs:492-563); mismatched item parity
  only warns, inherits first item's parity (coordinator/autonomous.rs:1141-1151).
- Collision between streams: nothing detects; overlaps simply summed (but see
  TX-F6: the modulator's own pairwise check then fails the whole bundle).

## B.2 Single-scorer verification

| # | Entry point | Path | Converges with display? |
|---|---|---|---|
| 1 | Autonomous self-CQ, Auto mode | `rank_candidates` + shared boost | **YES** — `placement_snapshot` (autonomous.rs:1107-1158) reads the same state; regression-tested (:2591+) |
| 2 | Autonomous self-CQ, **Hold mode (default)** | pinned `config.tx_offset_hz` (:1066-1068) | **NO** — display shows allocator ranking; decision ignores it |
| 3 | Autonomous pounce | allocator invoked (:1462) then **discarded** → Tx=Rx | **NO** — DX-proximity criterion dead in production |
| 4 | Manual StartQso / RespondToCaller / promotion | `compute_manual_tx_offset` | Separate scorer by design (T3); partially coupled via held candidate |
| 5 | Enter-park / click-park / `o` modal | picks from relayed snapshot slices (tui_runner.rs:1711-1752) | **YES** |
| 6 | Hound engage/QSY | hash | Separate by design |
| 7 | Auto-73 resend | raw Tx=Rx, no deconflict, ignores held offset (qso.rs:689-699) | NO (minor) |
| 8 | Stuck-DX hop | blind +300 | Separate by design (documented) |
| 9 | Collision jitter | `simple_jitter` | Separate; idle-only |
| 10 | Legacy fallback (no spectral) | `allocate_cq_frequency` | Not displayed (snapshot None in exactly this condition) |

**Verdict**: the CLAUDE.md invariant holds narrowly — display ≡ autonomous CQ
decision in Auto mode; park UI picks from the same snapshot. But default config
(Hold) and the most common autonomous action (pounce) decide outside the
instrument, and DX-proximity scoring never affects any transmission.

## B.3 Findings

**FQ-F1 — CONFIRMED, HIGH. Spectral frequency axis mismatched ~100–200 Hz.**
Bins span 0–3000 Hz (decoder.rs:8227-8231, `bin_start = 0`); coordinator labels
`freq_min_hz: 200.0` (coordinator/autonomous.rs:641-645). `power_near/peak_near`
(frequency.rs:59-100) read bin `(f−200)·3000/2800` — ~107 Hz low at 1500 Hz,
~179 Hz low at 500 Hz. Criteria 2–3 (35 of ~93 points) score the wrong
spectrum. [Re-verified by controller.]

**FQ-F2 — CONFIRMED (structurally; net effect race-dependent), HIGH. Parity
attribution of occupancy uses wall clock at feed time.**
`feed_decoded_messages_at` labels all records `SlotParity::current()`
(autonomous.rs:914-927) though each message carries the decoder's correct
`slot_parity`. Tick fires at slot boundaries (coordinator/autonomous.rs:591-599);
decodes completing before the boundary (the design target) are stamped with the
**next** slot's parity → E/O inverted; late decodes wait a tick and get the
correct parity. Per-slot occupancy (`clear_first`/`clear_second`, openness
strip, coverage warnings, repark hysteresis) inverted or mixed by decode
latency. Same wall-clock parity feeds `record_slot_activity` (:904-906) →
`SlotParityConfig::Auto` can systematically pick the busier slot. One-line fix:
use `msg.slot_parity`. [Re-verified.]

**FQ-F3 — CONFIRMED, HIGH. `own_frequencies` never populated in production.**
`register_qso_frequency`/`release_qso_frequency`: zero non-test call sites.
Criterion #7 (−50 own-separation) can never fire; `decide()`'s
`is_clear_of_own` (:1452) always true; placement snapshot's `own` list always
empty → BEST row can rank #1 a slice atop a live QSO stream. [Re-verified.]

**FQ-F4 — CONFIRMED, HIGH. Autonomous concurrent streams have no
minimum-separation enforcement.**
Pounce path (`classify_autonomous_opening` → `StartAutonomousQso` →
`respond_to_cq(dx, dx_freq, parity)`, qso.rs:2436-2453) latches the DX's
decoded frequency, no `deconflict_offset`, and (FQ-F3) no allocator-side
separation. `max_concurrent_qsos > 1` + two DXes within 75 Hz → overlapping own
streams summed into one waveform (and see TX-F6 whole-bundle failure). Only
manual calls deconflict. The allocator's pick (which had DX-proximity spacing)
is discarded.

**FQ-F5 — CONFIRMED, MED. Live-spot rarity nudge is dead code (unit mismatch).**
`spot_frequencies()` returns absolute RF Hz; boost compares to audio offsets
200–2800 with a <200.0 window (autonomous.rs:1048-1049) — never true on HF.
Even with correct units, +0.2 vs ~93 points is noise. The single-scorer
regression test uses synthetic offset-scale spots, masking the mismatch.

**FQ-F6 — CONFIRMED, MED. Operator's parked offset never reaches the autonomous
engine.**
Hold mode (default) returns `config.tx_offset_hz` (autonomous.rs:1066-1068,
wired from config at coordinator/autonomous.rs:396). `SetTxOffset`/Enter-park/
auto-repark write only coordinator atomics `tx_offset_hold_hz`/`tx_freq_mode`
(tui_relay.rs:1645-1687), consulted **only** by manual opens + WSJT-X UDP
status. Operator parks at 1480; autonomous CQs still at config 1500.
Auto-repark optimizes an offset autonomous never transmits on. Contradicts the
redesign spec ("park the TX offset there", §2).

**FQ-F7 — CONFIRMED, MED. Occupancy scoring floor bug.**
frequency.rs:279: `score += 15.0_f64.max(25.0 - activity as f64 * 5.0);` —
floors the busy-bin term at +15 regardless of activity. With criterion #4's
10-point cap, a bin with 10 decodes/both slots trails a clear bin by only 25
points — noise-floor + center-bias (FQ-F1-corrupted) can outvote real
occupancy. Intended `(25.0 - …).max(0.0)`. [Re-verified.]

**FQ-F8 — CONFIRMED, MED. Scoring is parity-blind.**
`score_candidate` uses both-slot activity; in-code "caller should filter if
needed" (frequency.rs:277-278) honored by no caller. A pounce should prefer
bins clear in *our* TX slot; a bin fully occupied in the harmless opposite slot
is penalized identically. Per-slot flags only color the TUI strip. The
2026-04-29 spec designed exactly this filter (`is_blocked_in_parity`); never
built — `TuiCommand::FindClearOffset` remains an unwired variant
(tui_runner.rs:311, 2347-2353).

**FQ-F9 — CONFIRMED, MED. TX-placement instrument + all occupancy tracking dead
when `autonomous.enabled = false`.**
Config-disabled branch spawns only a drain task (coordinator/autonomous.rs:
344-377): no spectral feed, no history, no `TxPlacementUpdate`. In-loop comment
"Sent regardless…" (:675-676) true only of the runtime Shift+Q gate. A
manual-only operator gets a permanently blank instrument — the redesign made it
the primary spectrum view for everyone.

**FQ-F10 — CONFIRMED, LOW-MED. Stuck-hop can land on another live stream.**
`stuck_hopped_offset` (+300 wrap, qso_manager.rs:84-93) does not consult
`active_tx_offsets()` — with concurrent QSOs the hop can land within 75 Hz of
another stream. Spectrum-blindness documented; stream-blindness not.

**FQ-F11 — CONFIRMED, LOW. Inconsistent passband constants.**
Allocator 200–2800 (frequency.rs:41); `TX_OFFSET_MIN/MAX` 300–2700
(qso_manager.rs:35-37; deconflict clamp + stuck-hop); jitter clamps 200–2800
(autonomous.rs:1575); spectral label 200–3000; `o` modal 200–2900. Concrete: a
held 250 Hz silently clamped to 300 with zero conflicts; allocator pick of
200–275 legal there, clamped on the manual path.

**FQ-F12 — SUSPECTED, LOW. Spectral snapshot staleness / self-TX contamination.**
(a) FT4's 7.5 s windows → 2 sends/tick vs bounded(2) channel + newest-dropped
`try_send` → snapshot lags 15–30 s; (b) windows covering our own TX slots feed
the snapshot — our own TX (if present in RX audio) penalizes our own offset
next tick; (c) `spectral_snapshot` never expires — waterfall stops ⇒ frozen
spectrum forever.

**FQ-F13 — CONFIRMED, LOW. Per-window min-max normalization makes "noise floor"
relative.**
Rows normalized 0..1 per window (ft8.rs:717-731): quiet band → pure noise
stretches to full scale (phantom structure); loud band → floor compresses. The
20+15-point spectral terms have inconsistent meaning across ticks (compounds
FQ-F1).

**Full-band behavior**: acceptable by design — soft scoring, no hard gates,
always 105 candidates, best-of-bad wins (crowded-band test :474-499). With
FQ-F7 though, "best" on a full band leans mostly on center-bias + misaligned
noise terms.

**Latched-offset drift audit (negative result — good)**: `metadata.frequency`
mutated mid-QSO only by the Auto-gated stuck-hop and the Hound QSY; auto-repark
provably never fires with an active QSO (double fail-closed read); jitter
suppressed while active and in Hold entirely. No silent re-pick path. One
fragility: stuck-hop leaves the state's embedded frequency pre-hop — that is
what keeps relevance matching (qso_manager.rs:3101-3111), an undocumented
load-bearing asymmetry vs Hound QSY.

## B.4 Doc inconsistencies (frequency)

1. CLAUDE.md "Single-scorer" — true only for autonomous-CQ-in-Auto vs display.
   Hold (default), pounce, and manual decide outside the displayed scorer.
   Reword or fix FQ-F6/F3/F4 to make it true.
2. TUI redesign spec §2 ("find the most open slice … park the TX offset there";
   "the display never diverges from what autonomous would actually pick",
   echoed in docs/DECISIONS/tui.md) — contradicted by FQ-F6 + FQ-F3. The
   rarity-boost "shared function" fix celebrated in tui.md is dead (FQ-F5).
3. 2026-04-29 waterfall-tx-offset spec — parity-aware filtering designed, never
   implemented (FQ-F8); vestigial no-op command variant remains; spec not
   marked stale.
4. docs/DECISIONS/tx-scheduling.md §hold+escape — accurate, but omits FQ-F10
   and the state-frequency non-update coupling.
5. Comment-vs-behavior: coordinator/autonomous.rs:675-676 false for
   config-disabled (FQ-F9); frequency.rs:277-278 caller responsibility no
   caller fulfills (FQ-F8); autonomous.rs:768 "replacement" claim while the
   legacy registry the smart allocator depends on was never wired (FQ-F3).
6. Config: `FrequencyAllocatorConfig.step_hz`/`.range` not plumbed from
   pancetta-config (`..Default::default()`, coordinator/autonomous.rs:421-429)
   — check docs/CONFIG.md doesn't advertise them.

---

# Track C — TX scheduling / parity / multi-TX

Files: `pancetta/src/coordinator/tx.rs` (read fully), TX-relevant parts of
`coordinator/qso.rs`/`mod.rs`/`autonomous.rs`, `pancetta/src/message_bus.rs`,
`pancetta-qso/src/qso_manager.rs` parity/admission,
`pancetta-ft8/src/modulator.rs` (multi-TX).

## C.1 Mechanism map

### Request creation (producers of `TransmitRequest`)
| Producer | Site | qso_id | tx_parity |
|---|---|---|---|
| QSO auto-sequence forwarder (all `MessageToSend`) | qso.rs:1681-1732 | Some | latched from metadata |
| TUI manual free-text send | tui_relay.rs:972 | None | None |
| Autonomous plan items (single) | autonomous.rs:1128 | Some/None | per-item |
| Autonomous plan items (bundle) | autonomous.rs:1141-1152 | — | first item's; mixed only **warned**, coerced |
| `--test-tx` injection | mod.rs:1519 | None | None |

`MessageToSend` emitters in qso_manager.rs: QSO open (:1314, :1609, :853/:994),
auto-sequence replies (:2212-2221), manual keep-call rearm (:3579-3712, 5 s
tick :3548, ≥15 s per QSO — `SLOT_SECONDS = 15` hardcoded, not
protocol-scaled), `send_message`/`resend_last_tx` :1731-1777,
`advance_existing_qso_to_step` :3382. Coordinator-side extra producer:
`maybe_auto_resend_73` (qso.rs:583-719) opens a new QSO via
`respond_to_caller(SeventyThree)`.

Every forward also writes `latest_tx_intent[key] = {text, freq, parity}`
(qso.rs:1701-1710) — the pivot source.

### Enqueue / channel
`MessageBus` = one bounded crossbeam channel per ComponentId
(message_bus.rs:971-1000); `send_message` (:1003-1067) `try_send`s;
**full channel = silent drop returning `Ok(())`**; expired dropped at send.
Single consumer: the TX worker's `tx_rx` (tx.rs:1047). No second reader — no
bus-level double-delivery.

### Worker loop (tx.rs:1108-2888)
1. Dequeue via `try_recv` + 10 ms idle sleep (:1165, :2872); `tx.recv_diag`
   logs (PR #159); protocol re-checked per cycle (:1147-1163).
2. `request_received_at` captured at pickup (:1218) — Symptom-B fix.
3. **Coalesce** (TransmitRequest heads only): sleep
   `coalesce_collect_window_ms` (800 ms FT8, protocol-scaled :87-91, :1247),
   then `coalesce_backlog_into` (:844-1006) drains queued TransmitRequests,
   stops at first non-TX message (re-enqueued to tail), runs pure
   `coalesce_transmit_requests` (:767-821): newest-per-qso_id wins, terminal
   dropped (live-set predicate), `qso_id=None` never coalesced/gated, cap
   `MAX_RETAINED_TX_STREAMS = 8` (:698). 1 survivor → single; ≥2 →
   `MultiTransmitRequest` with `bundle_parity = retained[0].tx_parity` (:979),
   origin Remote-if-any (:983).
4. **Single-TX arm** (:1288-1912): Step 0 policy hard-mute (:1308); Step 0a
   remote arm gate (:1351, fail-closed :586-594); Step 1 encode+modulate
   (:1435); Step 2 `resolve_required_parity` (:1487; fn :2894-2918: explicit
   wins; None → config Even/Odd/Auto-nearest) + `schedule_tx` (:127-169):
   current slot iff parity matches AND mstr ≤ `tx_late_max_ms` (8000); early →
   pad to slot+500 ms; late → cursor skip (mstr−500) ms; else defer 30 s;
   deferred branch re-checks liveness at defer time (:1522), marks TUI; Step 3
   audio build (:1570); Step 4 interruptible sleep to slot − ptt_lead (:1607);
   Step 4b `tx_qso_is_live` (:1640); Step 4b-arm remote re-check (:1682);
   **Step 4c pivot** (:1721-1770, `tx_pivot_target` mod.rs:118-130); Step 5
   PTT on (PttGuard :1773); Step 6 sleep to boundary (:1816); Step 7
   AudioOutput (:1836); Step 8 playback sleep; Step 9 PTT off; Step 10
   `TransmitComplete` (zero consumers).
5. **Multi-TX arm** (:1914-2731): same Step 0/0a; Step 0b per-item pre-encode
   liveness (:1998-2064); Step 1 `encode_and_modulate_multi_tx` (:492-572) —
   per-item encode, offset from `MULTI_TX_BASE_HZ = 200`, freq pushed in same
   iteration as encode (prior misalignment fixed); items rebound to
   `outcome.encoded_items` (:2154); `modulate_multi_tx`
   (modulator.rs:575-644) enforces pairwise ≥ bandwidth+25 Hz (**whole-bundle
   Err on violation**), sums, peak-normalizes 0.95; Step 2 parity/schedule
   identical (:2201-2214) — **`schedule.deferred` never consulted** (no
   defer-time recheck, no deferred flag, no counter — TX-F8); Step 4b key-time
   `live_mask` over `encoded_qso_ids` (:2306): all stale → drop; all live →
   fast path; partial → re-encode live subset, reuse schedule (:2410-2547);
   Step 4b-arm remote re-check (:2556); PTT → slot sleep → summed audio →
   per-item TransmitComplete. **No Step-4c pivot in the multi arm.**
6. **Tune arm** (:2734-2867): policy-gated, immediate PTT, sine.

### Shared drop-stale state
`active_tx_qsos: Arc<RwLock<HashSet<String>>>` keyed `active_tx_qso_key`
(mod.rs:66). Sync from QsoEvents: insert on `StateChanged→active` (qso.rs:1459);
remove immediately on `Failed` (:1466) + `QsoFailed` (:1958); on `QsoCompleted`
insert + spawned 45 s grace purge (:1776-1823). `tx_qso_is_live` (mod.rs:92,
wrapper tx.rs:395-403) **fails OPEN on poisoned lock** (tested :3250); remote
arm gate fails CLOSED (tested :3993).

### Parity latch / admission (pancetta-qso)
- CQ paths latch None→concrete once (`latch_cq_parity_if_none`, :751-765).
- Respond paths: `dx_parity.map(opposite)` — None stays None (TX-F2).
- Admission: pure `admit_new_qso` (:505-523): idle→Admit, unpinned→Admit,
  same→Admit, cross→Queue. `current_tx_side()` (:534-539) = first active QSO
  with latched parity. Gated at: manual StartQso (qso.rs:2254-2313 →
  `PendingManualCalls`, promoted :377+), StartAutonomousQso (:2419),
  EngageHound (:2505), `maybe_answer_caller` (:889). **Not gated**: manual
  RespondToCaller (:2607-2662) and auto-73 (:689).

## C.2 Invariant check

- **Same-parity at admission**: enforced for 4 of 6 opening entry points.
  RespondToCaller + auto-73 unchecked — self-consistent only if callers are
  always decoded in our RX window (fresh decodes yes; a stale Callers-panel row
  after a side flip can cross). Also `current_tx_side()`+`admit` is a TOCTOU
  pair across independent tasks — two concurrent opens from idle can both see
  `None` and latch opposite parities (same residual-race class
  `try_switch_operating_mode` documents at mod.rs:330-343).
- **Same-parity at coalesce time: NOT enforced.** Bundle keyed to
  `retained[0].tx_parity` (tx.rs:979) with no agreement check;
  autonomous.rs:1141-1151 warns and coerces. Combined with TX-F2 this is a
  live hole, not defense-in-depth.
- **Drop-stale gate**: confirmed fail-open, deliberate, documented. Acceptable:
  a poisoned lock also freezes the producer side (inserts use `if let Ok`), so
  fail-open = "keep transmitting per last-known state" until Shift+Q.
- **No frame in a wrong-parity window**: `schedule_tx` never targets a slot of
  the wrong *requested* parity (tests :2982-2993) — but the requested parity is
  only as good as the request's `tx_parity` (TX-F2, TX-F5). Manual
  `qso_id=None` sends with Auto self-parity take the nearest slot by design —
  may be the listening window of active QSOs; nothing warns.

## C.3 Findings

**TX-F1 — CONFIRMED, HIGH: Step-4c pivot + still-queued newer request = the
double-PTT-for-73.**
Mechanism: pivot (:1721-1770) rewrites an in-flight, dequeued frame to the
freshest intent at key time; nothing consumes/cancels the queued
`TransmitRequest` carrying that same text. Timeline (our parity Odd):
1. Rearm re-emits R-report at :35; worker dequeues, coalesce window (:35.8)
   finds nothing, sleeps toward :45. (30 s-defer variants widen the window.)
2. DX's RR73 decodes ~:43; state machine emits the 73 **once** (matches #158),
   enqueues request R2, updates intent to "73", QSO → Completed → 45 s grace.
3. :44.8 — Step 4b passes (grace), Step 4c pivots the R-report to "73" →
   **PTT #1 at :45**.
4. Worker frees ~:58, dequeues R2 ("73"); mstr ≈ 13 s > 8 s → defers to 1:15;
   Step 4b still passes (grace purge ~1:28); pivot no-ops (texts equal,
   mod.rs:125) → **PTT #2, identical 73 at 1:15** — consecutive same-parity
   slots.
Reproduces the observed signature exactly: one `MessageToSend(73)`, two
dequeues with different msg ids and different original texts (why #159's
same-id/same-content searches were empty), two PTTs. The `tx.pivot` info line
(:1751) is the smoking gun for the next occurrence — currently invisible at
default EnvFilter. The 16 s→45 s grace widening is what lets leg #2 survive
Step 4b. Fix direction: after a pivot, record (qso_key, pivoted_text) and drop
a subsequently dequeued request matching it (or tombstone the intent and treat
a Completed-in-grace QSO's request as consumed-once).

**TX-F2 — CONFIRMED, HIGH: responder paths never latch parity when
`dx_parity = None` → alternating TX windows.**
(= SM-F2; see Track A. Spec 2026-04-27 §218-224 promised first-decode
refinement — never implemented. Also enables TX-F5.)

**TX-F3 — CONFIRMED, MED: single-worker head-of-line blocking; a deferred
request stalls every other stream up to ~43 s.**
One message per arm with in-arm sleeps (pre-PTT up to 30 s + 13 s TX). A
keep-call landing >8 s into its own slot defers 30 s and occupies the worker;
other QSOs' replies sit in the channel, miss their slot, air a cycle later.
Structural root of remaining Symptom-C behavior.

**TX-F4 — CONFIRMED (mode-scoped), MED: Step-4c pivot encodes FT8 regardless of
protocol.**
:1733-1735 uses legacy `encode_message`/`modulate_symbols`, not
`encode_for_protocol`/`modulate_for_protocol`. In FT4/FT2 a pivot emits an FT8
waveform onto the FT4 grid — the exact bug class the protocol wiring fixed
(tests :3600-3663), resurfacing in one path. FT8 byte-identical today.

**TX-F5 — SUSPECTED, MED: bundle-parity coercion can key a wrong-parity item.**
tx.rs:979 + autonomous.rs:1142-1151 stamp the bundle with the first item's
parity. Given TX-F2 (None-parity first-retained) or the admission TOCTOU, a
latched-Odd frame can key in an Even slot (the DX's own window). Comment at
:977 says "freshest stream's parity"; `retained[0]` is actually first-seen/
oldest — comment/code mismatch.

**TX-F6 — SUSPECTED, MED: coalesce-fold can turn two individually-transmittable
streams into a whole-bundle failure (separation livelock).**
`modulate_multi_tx` errors the entire bundle when any pair < bandwidth+25 Hz
(modulator.rs:586-601). Coalescer folds with no separation pre-check; two
Tx=Rx caller-answers <75 Hz apart (`maybe_answer_caller` passes Tx=Rx with
**no** deconfliction, qso.rs:934-946) fail *both* every slot they coalesce,
retried by keep-call until the watchdog retires them. Sent individually they'd
be fine.

**TX-F7 — CONFIRMED, LOW-MED: late-cursor audio emitted time-shifted by the
collect window without cursor compensation.**
`request_received_at` (pickup) drives cursor math, but emission happens ≥800 ms
later (collect sleep) — transmitted symbols ~0.8–1 s later than the cursor
assumed → extra effective DT on every late-start TX. Tolerable FT8 (±2.5 s
window), tighter FT4 even with the 400 ms scaled window. Fix: recompute cursor
from actual emission time (or subtract the sleep from viability only).

**TX-F8 — CONFIRMED, LOW: multi-TX arm ignores `schedule.deferred`** — no
defer-time liveness recheck (single has :1522), no TUI deferred flag
(:2067-2079 always false), no `TX_DEFERS_COUNT`. A deferred bundle looks
alive-but-silent up to 30 s, relies solely on the key-time gate.

**TX-F9 — CONFIRMED, LOW: bus `send_message` silently returns `Ok(())` on
full/absent channel** (message_bus.rs:1047-1064). A dropped TransmitRequest
loses a TX cycle with only a warn; a dropped `SetPtt{false}` in
`PttGuard::drop` would strand PTT (guard logs, no retry). Also
`coalesce_backlog_into`'s re-enqueue of a non-TX message (:892) reorders it
behind later arrivals and can drop it if the channel is full.

**TX-F10 — CONFIRMED, LOW (design tension): auto-73 duplicate-73 overlap.**
(= SM-F3 shape; bounded 3×/14 s, RR73-triggered — mostly correct behavior, but
can produce back-to-back 73s that mimic TX-F1. Ruled out for the traced
occurrences: it would have shown a second `MessageToSend` in #158.)

**TX-F11 — CONFIRMED, LOW: keep-call rearm cadence hardcoded 15 s**
(qso_manager.rs:3582) — every-other-slot in FT4; not protocol-scaled unlike the
coalesce window.

**TX-F12 — noted: mode-switch admission race** already documented as accepted
residual (mod.rs:330-343).

### Symptom C residual gap
The 800 ms collect window batches only openings within 800 ms of head pickup.
Serial keypresses are 1–3 s apart → realistic behavior: stream 1 alone in slot
1; streams 2..N coalesce while stream 1's arm occupies the worker (~13 s) and
fire together in slot 2; stream 1's keep-call joins slot 3. Big improvement
over one-per-cycle, still not "all N first window"; and per TX-F3, if the head
request *defers*, siblings are blocked ~43 s — *worse* than pre-fix for that
case. The collect sleep is non-interruptible (≤800 ms shutdown/abort latency)
and per TX-F7 silently converts into on-air DT on late starts.

## C.4 Doc inconsistencies (scheduling)

1. Spec 2026-04-27 §222-224 "first received decode will refine the parity" —
   never implemented (TX-F2); its fallback framing also wrong post-BUG-1: CQs
   latch once, cluster-pounce QSOs re-resolve per emission.
2. docs/DECISIONS/tx-scheduling.md:24 claims defer-time liveness recheck for
   both `TransmitRequest`/`MultiTransmitRequest` — multi arm has none (TX-F8).
3. tx-scheduling.md:16 + spec :203-211 assert bundles "naturally share one
   parity" — assumption, not enforcement (TX-F5), falsifiable via TX-F2.
4. tx-scheduling.md:12 / wiki parity-rule present admission gating as covering
   manual picks — manual `RespondToCaller` (qso.rs:2607) has no gate.
5. wiki/pages/parity-rule.md:37 cites `resolve_required_parity` at tx.rs:2201 —
   actual :2894 (line drift; :2201 is the multi arm's call site).
6. tx.rs:977 comment ("freshest stream's parity") contradicts code
   (first-seen).
7. Spec's `ptt_lead_ms` 80 ms + defer notice implemented as described — single
   arm only (TX-F8).
8. CLAUDE.md drop-stale invariant — true, with the multi-arm caveat (check via
   `encoded_qso_ids`/rebuild) and the wording eliding deliberate fail-open
   (tx-scheduling.md:24 documents it correctly).

---

## Post-remediation live finding — SM2LIY/C6AVD multi-TX incident (same day)

A live on-air incident surfaced the same evening the 5-batch remediation above
landed, exercising two gaps the remediation explicitly didn't cover (neither
is a regression of Batch 1-5). The operator was mid-QSO with SM2LIY (manual,
single-TX) and added C6AVD as a second same-parity manual QSO (multi-TX). When
SM2LIY's QSO advanced (received RR73, emitted its closing 73) WHILE its frame
was already bundled into a `MultiTransmitRequest` alongside C6AVD's frame, the
bundle transmitted the STALE pre-advance text verbatim — twice, once per
bundle cycle, re-sending the DX a stale "R-14" report after they'd already
sent RR73. Separately, the operator invoked the manual "send 73"
(`respond_to_caller(SeventyThree)`) 3 times in 8 seconds trying to recover;
each invocation built a BRAND NEW `QsoId`, independently completed and
logged its own ADIF entry, and emitted its own real 73 — 4 duplicate SM2LIY
log entries across the incident.

**Root cause 1 — no bundle-arm pivot.** Batch 1's Step-4c late pivot
(TX-F1/SM-F... mechanism above) only exists in the single-`TransmitRequest`
arm; the `MultiTransmitRequest` arm never consulted `latest_tx_intent` at
key-time, so a bundle's items were transmitted exactly as encoded at Step 1
regardless of how stale they'd become during the pre-PTT wait.

**Root cause 2 — no grace-window idempotency on manual close.**
`respond_to_caller`'s FIX 1 dedup (SM-F... "if we already have an ACTIVE
manual QSO...") only matches a still-ACTIVE QSO; once a QSO reaches terminal
`Completed`, a subsequent `SeventyThree` reply for the same station fell
straight through to "build a new QSO" with no idempotency guard at all.

**Fix (this batch — same session, landing right after the 5-batch
remediation above):** (A) new `pivot_bundle_items` (`coordinator/mod.rs`) is
the bundle-arm analogue of `tx_pivot_target`; `tx.rs`'s multi-TX arm's Step
4b now pivots the still-live subset before deciding fast-path vs. rebuild (a
pivot alone now forces the rebuild path), plus a bundle-side
"Step 0-dup" tombstone gate mirroring Batch 1's `is_pivot_duplicate`. (B) new
`find_recently_completed_manual_qso_for` in `pancetta-qso/src/qso_manager.rs`
gives `respond_to_caller` a grace-window (`COMPLETED_QSO_REWORK_GRACE`, 45s,
shared with the coordinator's drop-stale-TX `completed_tx_grace`) idempotent
close: a close-step (`Rr73`/`SeventyThree`) reply for a station just
completed within the grace window re-keys the existing QSO instead of
spawning a sibling; Grid/Report-step replies are left as the deliberate
legitimate-rework path. See `docs/DECISIONS/tx-scheduling.md` ("Multi-TX
bundle-arm pivot + tombstone") and `docs/DECISIONS/qso-engine.md`
("Grace-window idempotent close") for the landed design; the fix commit(s)
land in this same session.
