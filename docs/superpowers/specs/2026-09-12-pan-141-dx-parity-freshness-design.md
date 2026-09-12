# PAN-141: DX-parity freshness check before key-up — design

**Ticket:** PAN-141 — a live, log-confirmed incident (2026-09-12, calling 5Z4VJ). Operator
manually called 5Z4VJ (heavy pileup, cycling callers ~every 30s) via DX Hunter, based on a real
local decode of 5Z4VJ working N3UL on Even. `respond_to_cq_with` correctly latched our
`tx_parity = Odd` (opposite of that decode) at QSO-open. ~20-30s later, at our actual key-up,
5Z4VJ had already moved on to answering NV1U — and that exchange put THEM on Odd too. Our reply
landed in the exact slot 5Z4VJ was using for a different caller; they never heard us.

Root cause: `tx_parity` is latched once from a single observation at QSO-open time and never
re-validated against a fresher decode before the actual key-up. The math was correct for the data
available at latch time; the data was stale by key-time. This is a ~20-30s structural gap (the
wait for the next valid opposite-parity slot), not a bug in the parity arithmetic itself.

## Confirmed against current `main`

Re-checked against `71e705e` (current `main` tip) — nothing has moved since the incident:

- `respond_to_cq_with`'s parity latch is `pancetta-qso/src/qso_manager.rs:2186-2189` (line numbers
  match the ticket exactly).
- `git log --grep=tx_parity` shows the most recent touches are PAN-72 (`bc05594`,
  `13a56a0` — scores an offset switch against a QSO's *existing* `tx_parity`, doesn't re-derive
  it) and a multi-TX bundle-parity preservation fix (`880a914`) — neither changes the latch-once
  behavior this ticket is about.
- `tx_parity_provisional` (`process_message_for_qso`, qso_manager.rs:3860-3885) is a *different*
  mechanism: it only refines a latch that started as `None` (no live decode at QSO-open, e.g. a
  cold DX-Hunter/cluster spot), and only fires once, triggered by a frame **from the partner
  addressed to us**. In PAN-141, `dx_parity` was `Some` (a real decode existed), so
  `tx_parity_provisional` was `false` from creation — this refinement path never runs for this
  incident, and wouldn't help even if it did (it fires on the DX's reply *to us*, which is already
  too late to prevent the collision).

## Existing mechanisms this is adjacent to (and must not fight)

1. **Half-duplex single-shared-parity invariant** — `admit_new_qso`/`current_tx_side`
   (qso_manager.rs:1133-1217). Every concurrent *active* QSO must transmit on the same parity; a
   new QSO whose desired parity conflicts with the live side is `Queue`d, never admitted
   cross-side. `tx_parity` is otherwise treated as immutable once latched (the only mutation site,
   `tx_parity_provisional`'s one-shot refinement, runs before a QSO has any real traffic). **Any
   fix that re-latches `tx_parity` on an already-active QSO risks silently desyncing it from
   `current_tx_side()`** if `max_concurrent_qsos > 1` — our own concurrent streams would then
   collide with each other's RX windows, which is strictly worse than what PAN-141 reports.
2. **Step 4b drop-stale-TX gate** (`coordinator/tx.rs:5651-5680`, mirrored for multi-TX bundles at
   `multi_tx_bundle_still_fully_live`, ~line 2748) — the *only* existing precedent for "re-check
   something immediately before PTT and skip this cycle if it fails," per AGENTS.md's "drop-stale-TX"
   invariant. It runs strictly **after** the pre-PTT sleep completes and **before** `PttGuard`
   engages PTT — never against an already-keyed transmission.
3. **PAN-140** (PR #369, sibling ticket, same log session): a stall-triggered offset switch aborted
   an *already in-flight* PTT mid-frame, garbling both transmissions. The lesson: any new
   pre-key check must hook at the same "not yet keyed" choke point Step 4b already uses, never at
   a point that can race a live PTT. `pancetta/src/coordinator/autonomous.rs`'s
   `drain_pending_qso_offset_requests` (PAN-140's fix) is untouched by this ticket.
4. **`CrossTimeState::a7_recent_calls`** (`pancetta-qso/src/cross_time_state.rs:296-375`) —
   **SHIPPED-INFRA, currently unconsumed** (its own module doc: "no consumer reads them until a
   downstream hypothesis adds the read path"). Every post-FP-filter decode (`ft8.rs:2358`, *every*
   decode this station makes, regardless of whether it's relevant to any active QSO) already
   records `{callsign, freq_hz, slot_parity, decoded_at}` here, keyed by callsign, one entry per
   callsign (newest wins). **This is exactly the "DX's freshest observed own-TX parity" data
   PAN-141 needs, already being collected, with zero consumers today.** No new decode-tracking
   plumbing is needed — this ticket is the first consumer hb-048's infra doc anticipated.

   Caveat: `A7RecentCallTable::get()` does **not** filter by age — eviction is lazy, on the next
   `record()` call, not on read. A caller must check `decoded_at`'s age itself; the table's own
   30s `max_age` is tuned for hb-048's decoder-correlation purpose, not TX-collision avoidance,
   and shouldn't be assumed by a new consumer.

## Options considered

1. **(Recommended.)** At the exact Step 4b choke point (pre-PTT, post-sleep), look up the DX's
   freshest observed own-TX parity from `a7_recent_calls`. If it's fresh (within a new,
   purpose-specific window) and equals the parity we're about to key into (i.e., DX is currently
   transmitting to someone else in the very window we'd use to reach them), **hold**: skip this
   cycle exactly like the existing drop-stale-TX gate does (no PTT, `TransmitComplete{success:
   false}`, diagnostic log), and let the QSO's existing retry/watchdog cadence (`rearm_manual_calls_at`,
   `manual_call_max_calls`, `report_timeout`) handle what happens next. **Never mutates
   `tx_parity`.**
2. **Re-latch `tx_parity` from the freshest decode at key-time instead of QSO-open time.**
   Rejected as the *sole* fix: latching happens once and doesn't reschedule an already-pending key
   event mid-cycle (TX still happens at the fixed 15s boundary we're already waiting on) — so on
   its own it does nothing to prevent the immediate collision PAN-141 describes. Combined with (1)
   it could improve *future* cycles' scheduling, but doing that safely for an *active* QSO requires
   routing through the same Queue/Admit machinery `admit_new_qso` already enforces for *new* QSOs
   (§"Existing mechanisms" #1) — a materially bigger, riskier change than this incident's scope
   (a single manual DX-Hunter call, typically with zero or few other concurrent QSOs) justifies.
   Not pursued in this PR; flagged as a possible follow-up if operator experience shows repeated
   holds against a DX that has durably settled onto a new pattern.
3. **Surface staleness to the operator only, no TX behavior change.** Insufficient alone: the
   whole point of the incident is an *autonomous* transmission colliding on-air with a third
   station; an indicator the operator might not be watching in real time doesn't prevent that.
   Folded into (1) as a log/diagnostic line (reusing the exact `emit_diagnostic` call the
   sibling stale-TX-drop arm already makes), not pursued as a replacement.

## Design (Option 1)

### Freshness window

New constant, **not** reusing `a7_recent_calls`'s own `max_age` (different tuning purpose, per the
caveat above): `2 × active_slot_ns` — self-scaling with the live protocol exactly like PAN-72
round 7 finding 5's rearm-cadence fix (FT8 → 30s, FT4 → 15s, FT2 → 6.4s), read from the same
`active_slot_ns` atomic the TX worker already has in scope. This is the same order of magnitude as
the ~20-30s gap the incident actually measured, and needs no new config knob.

### Gate logic (new pure function, `coordinator/tx.rs`, alongside `tx_qso_is_live`)

```rust
/// `None` from the lookup (no fresh entry, callsign unknown, or entry older
/// than the freshness window) always means "proceed" — this gate only ever
/// blocks on POSITIVE fresh evidence of a collision, never on missing data.
fn dx_parity_conflict(
    their_callsign: &str,
    required_parity: pancetta_core::slot::SlotParity,
    cross_time_state: &CrossTimeState,
    now: SystemTime,
    freshness_window: Duration,
) -> bool {
    let Ok(a7) = cross_time_state.a7_recent_calls.read() else { return false; }; // poisoned → fail open, matches tx_qso_is_live
    let Some(entry) = a7.get(their_callsign) else { return false; };
    let Ok(age) = now.duration_since(entry.decoded_at) else { return false; };
    if age > freshness_window { return false; }
    slot_parity_from_u8(entry.slot_parity) == required_parity
}
```

Called from **two** sites, mirroring exactly where `tx_qso_is_live` is already called at Step 4b:

- Single-TX path, `coordinator/tx.rs:~5661` (immediately alongside the existing
  `tx_qso_is_live` check, same `if` cluster, same "drop and `continue 'worker`" shape).
- Multi-TX bundle path, alongside `multi_tx_bundle_still_fully_live` (~line 2748) — per-item,
  same as that function's own per-item `tx_qso_is_live` loop, so a bundle with one colliding item
  re-encodes the surviving subset exactly the way a mid-flight-stale item does today.

Scope: only for a `qso_id` whose QSO has an **established** `their_callsign` (`get_qso` returns
`Some`, `metadata.their_callsign: Some(_)`). A `CallingCq`/manual send with no target yet has
nothing to look up and is unaffected — this only ever gates a TX aimed at a specific station.

### Handle plumbing

- `cross_time_state: Arc<CrossTimeState>` — plain `Arc` clone into `start_transmitter_component`'s
  spawn closure alongside `active_tx_qsos` etc. It's coordinator-owned (`ApplicationCoordinator::new`),
  independent of the Qso component's supervised-restart lifecycle, so no watch/refresh indirection
  is needed (unlike `QsoManager` below).
- `QsoManager` handle (needed to resolve `qso_id → their_callsign`) — **must** use the existing
  `qso_manager_watch: tokio::sync::watch::Sender<Option<QsoManager>>` pattern PAN-72 introduced for
  exactly this reason: the TX worker's spawn closure captures its handles once, but `Qso` is
  independently supervised-restarted (AGENTS.md), and a plain spawn-time clone would go stale
  across a restart the same way the Autonomous task's pre-PAN-72 clone did. Subscribe once at
  `start_transmitter_component` spawn time, `.borrow().clone()` fresh at the Step 4b choke point
  (not cached across the sleep-to-PTT wait, so a mid-wait restart is still handled correctly).
  `None` (Qso component not up) degrades to "proceed" (fail open, same posture as every other
  best-effort lookup in this gate) — a genuinely dead Qso component means `active_tx_qsos` is
  also stale and `tx_qso_is_live` is already the authoritative gate in that state.

### On hold

Identical shape to the existing stale-TX-drop arm immediately above it: `info!` at
`target: "pancetta::tx.policy"`, `emit_diagnostic` (so it surfaces on the existing TUI diagnostic
feed for free — no new UI widget in this PR), `send_tx_queue_status(&message_bus, None, Vec::new())`,
a `TransmitComplete{success: false}`, `continue 'worker`. No QSO state mutation, no `tx_parity`
change, no new failure-reason/counter. The QSO's own existing watchdogs
(`manual_call_max_calls`, `report_timeout`, `AUTO_RESEND_MAX_CALLS`) already own "give up on an
unanswered QSO over time" — this gate only ever prevents *one* doomed transmission per cycle, not
a new termination path.

## Explicitly out of scope / invariants preserved

- `tx_parity` is **never** mutated by this mechanism — `current_tx_side()`'s single-shared-parity
  invariant across concurrent QSOs is untouched (see Option 2 above).
- Never aborts an in-flight PTT — this hooks at the exact same "not yet keyed" choke point Step 4b
  already uses; it cannot recreate PAN-140's race shape by construction.
- `manual_call_max_calls`/`call_count` accounting is unaffected by this change: `call_count` is
  already incremented at message-construction time, *before* Step 4b runs, so a held cycle still
  "spends" a call attempt with nothing transmitted — this is the exact same accepted tradeoff the
  pre-existing stale-TX-drop gate already has, not a new one introduced here.
- `pancetta/src/coordinator/autonomous.rs`'s `drain_pending_qso_offset_requests` (PAN-140's fix) —
  untouched.
- PAN-142 (drift-confirm operator visibility, P3) — separate ticket, not folded in beyond the
  `emit_diagnostic` line this gate already needs for its own sake.
- No new dedicated TUI indicator in this PR (diagnostic-feed log line only) — a follow-up ticket
  if on-air experience shows operators want a persistent visual cue distinguishing this hold from
  other drop reasons.

## Open questions for sign-off

1. Freshness window: hardcoded `2 × active_slot_ns` (recommended, no new config surface) vs. a new
   `[qso]`/`[autonomous]` TOML knob. Recommend hardcode — the incident-measured gap (~20-30s at
   FT8's 15s slot) is exactly `2×`, and PAN-72 established the self-scaling-by-protocol precedent.
2. Multi-TX bundle scope: include the per-item gate in this same PR (recommended, for symmetry
   with `tx_qso_is_live`'s own single+multi coverage) vs. defer to a fast-follow scoped to just the
   single-TX manual-call path the incident actually hit.
3. Anything beyond the diagnostic-feed log line for operator visibility in v1, or is that
   sufficient pending on-air validation?

## Testing plan

- `pancetta` (`coordinator/tx.rs`): pure-function unit tests for `dx_parity_conflict` (fresh+same
  parity → conflict; fresh+opposite parity → no conflict; entry older than the freshness window →
  no conflict; no entry → no conflict; poisoned lock → no conflict/fail open), mirroring
  `tx_qso_is_live`'s own test style (`coordinator/mod.rs:2534-2585`).
- `pancetta` (`tests/coord_sim.rs`): a scenario reproducing PAN-141 exactly, using the **real**
  `QsoManager`/`CrossTimeState`/Step 4b gate the harness already drives — decode DX→N3UL on Even,
  manually call the DX (latches `tx_parity = Odd`), decode DX→NV1U on Odd inside the freshness
  window, advance to key-up, assert **no PTT keys**; a control case where that second decode is
  aged past the freshness window asserts PTT **does** key (the fix must not become a blanket
  "never call a pileup DX" gate).
- `cargo test --workspace --features transmit --exclude pancetta-research` and
  `cargo clippy -p pancetta -p pancetta-qso --all-targets -- -D warnings` clean before opening for
  review.
- No on-air/loopback coverage possible for a genuine fast-cycling pileup DX — flagged for an
  on-air sanity check after merge, consistent with recent PAN tickets (PAN-72, PAN-108, PAN-91/92).
