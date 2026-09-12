# PAN-148: final atomic pre-PTT gate (single-TX and multi-TX) — design

**Ticket:** PAN-148 — a follow-up filed during PR #370's (PAN-141) review, spanning two findings:

- **Finding 1** (Codex P1, round 2, multi-TX): the partial-staleness "report dropped items, then
  re-encode survivors" branch emits per-item diagnostic/`TransmitComplete` **awaits** after
  `live_mask` is computed. A surviving item can go stale (QSO ends, or its DX moves onto the
  colliding parity) while one of those awaits is pending, and nothing re-checks it before PTT.
- **Finding 2** (Codex P2, round 4, both paths): the single-TX path performs Step 4c's
  pivot/remodulation and the Step 5 status-announce awaits (`send_tx_status`,
  `send_tx_queue_status`) *after* the last liveness/DX-parity check and *before* the actual
  `SetPtt{true}` send. The multi-TX path has the identical gap before its own Step 5.

Both are the same defect shape: AGENTS.md's drop-stale-TX invariant ("the worker re-checks QSO
liveness at the last instant before PTT") was satisfied for "immediately before Step 4b-arm," not
for "immediately before the actual key." Step 4c/Step 5's several `.await` points sit in between.

## Confirmed against current `main` (`e8a5c69`, PR #370 merged)

Re-read `pancetta/src/coordinator/tx.rs` end to end for both paths; line numbers below are current,
not the ticket's (PR #370-era) numbers, which have shifted:

- **Single-TX:** Step 4b-parity (DX-parity, `tx.rs:5830`) and Step 4b (liveness, `tx.rs:5884`) are
  the last liveness/parity evidence read before Step 4c's pivot/remodulation (`tx.rs:6089-6227`,
  fully synchronous — confirmed no `.await` in that block) and Step 5's three awaits: `PttGuard::new`
  (`tx.rs:6230`), `send_tx_status` (`tx.rs:6241`), `send_tx_queue_status` (`tx.rs:6243`). Step
  4d-arm (`tx.rs:6268`) re-checks only the remote arm, immediately before `SetPtt{true}`
  (`tx.rs:6321`) with zero intervening awaits on its success path — that part is already correct;
  liveness and DX-parity are simply never included in that final re-check.
- **Multi-TX:** Step 4b (`tx.rs:8117-8138`) computes `parity_hold_mask`/`live_mask` once. The
  partial-staleness branch (`tx.rs:8246-8427`) emits per-item `emit_diagnostic`/`TransmitComplete`
  awaits (`tx.rs:8268-8311`) and, on rebuild, `emit_tx_failure_diagnostic` awaits
  (`tx.rs:8332-8352`) — all **after** `live_mask` was read. Step 5 (`tx.rs:8608-8641`) has the same
  three awaits as the single-TX path. Step 4d-arm (`tx.rs:8655`) again re-checks only the arm,
  immediately before `SetPtt{true}` (`tx.rs:8710`) with zero intervening awaits on success — same
  partial fix as single-TX, same missing liveness/parity coverage.

`encode_and_modulate_multi_tx` (`tx.rs:2973`) — confirmed **fully synchronous**, no `.await`
anywhere in its body (it calls `encode_for_protocol`/`modulate_multi_tx`, both sync). This matters
for the design below: a bundle re-encode can run at the true last instant with no new await window.

## Existing mechanisms this is adjacent to (and must not fight)

1. **Step 4d-arm** (both paths) — the *only* existing precedent for "recheck immediately before
   `SetPtt`, zero awaits after." Its own doc comment already explains why it sits where it does
   (round-2's placement before Step 5's awaits left exactly this gap for the arm check; round-3
   moved it to the true last instant). This ticket extends the SAME choke point to cover
   liveness/DX-parity — it does not add a second, differently-timed check.
2. **PAN-141's Step 4b-parity gate** — untouched. This ticket does not change when or how the
   *first* liveness/parity check runs; it adds the missing *second* (final) one.
3. **PAN-140/PAN-143** (same root-cause class: check-then-act against TX-worker state with
   intervening awaits, no shared lock) — this is the third ticket in that family. Per
   `feedback_escalate_before_deep_architecture_change`, the fix here is deliberately the smallest
   design that closes the actual gap, not a broader synchronization mechanism (e.g. holding a lock
   across Step 4c/5) — see "Options considered" below for why a lock was rejected.
4. **Double-PTT `pivoted_once` tombstone bookkeeping** — Step 4d-arm's denial branches already
   remove a tombstone Step 4c/4b-pivot inserted this cycle if nothing reached the air. The new
   final-gate denial branches must do the same (nothing new conceptually, just extended to two more
   denial reasons).

## Options considered

1. **(Recommended.) Extend Step 4d-arm into "Step 4d: final atomic pre-PTT gate,"** re-checking
   liveness and DX-parity in the same call, immediately before `SetPtt{true}`, with the SAME
   "zero-`.await`-after" placement Step 4d-arm already established for the arm check. On any
   failure, deny exactly like Step 4d-arm's existing denial branches: unwind the
   already-announced-but-not-yet-hardware-asserted `ptt_active`/`ptt_guard`/TX-badge/queue-status,
   remove any `pivoted_once` tombstone from this cycle, diagnostic + `TransmitComplete{success:
   false}`, `continue`. For multi-TX, if the final per-item mask differs from "every item still
   live and unheld," **abort the whole bundle this cycle** rather than attempting a second
   synchronous re-encode of the shrunk survivor set.
2. **Multi-TX: synchronously rebuild survivors at the final gate and key them, deferring the
   dropped item(s)' diagnostic/`TransmitComplete` notifications until after the PTT-on send** (the
   ticket's own "suggested fix direction"). Rejected for this PR: `encode_and_modulate_multi_tx`
   being synchronous makes this *possible*, but it adds a second re-trim/re-cursor/re-announce path
   through exactly the code region that has now taken three separate review-round findings across
   two tickets (PAN-141, PAN-148) — more moving parts in the highest-risk file in the repo, to save
   a sub-millisecond-window survivor from waiting one more ~15s cycle. The window this final gate
   actually protects is the handful of near-instant in-process channel sends between Step 4b and
   Step 5 (no I/O, no sleep) — several orders of magnitude narrower than the ~20-30s pre-PTT sleep
   Step 4b itself already covers with the full rebuild-and-save-survivors treatment. Option 1's
   "abort the whole bundle" is strictly safe (never keys stale/held evidence) and keeps the two TX
   paths structurally symmetric (single-TX is already all-or-nothing, having exactly one item).
   Flagged as a possible future refinement if telemetry ever shows this exact abort firing often
   enough to matter — not expected, given the window size.
3. **Hold a lock (e.g. the existing `ptt_sync_gate` from PAN-143) across Step 4c/Step 5 so nothing
   can mutate `active_tx_qsos`/`a7_recent_calls` while the worker is mid-announce.** Rejected: PTT
   itself is single-flight per rig, and `ptt_sync_gate` exists to serialize concurrent worker
   invocations' PTT transitions (PAN-143), not to freeze unrelated shared state (`active_tx_qsos`
   is written by the independently-supervised `Qso` component; `a7_recent_calls` by every decode).
   Doing this would mean the TX worker holding a lock across QSO-lifecycle and decoder writes for
   the ~1-5ms Step 4c/5 announce window — a much bigger behavioral change (risk of the exact
   priority-inversion/stall shape PAN-140 was about) for no benefit over "just recheck fresh state
   right before the key," which is the pattern this file already uses everywhere else (Step 4b,
   Step 4b-arm, Step 4d-arm).

## Design (Option 1)

### Shared recheck functions (new, `coordinator/tx.rs`)

Two new functions, alongside the existing `tx_qso_is_live`/`dx_parity_conflict_for_qso`/
`multi_tx_bundle_still_fully_live` they compose — no new algorithm, just naming the "read
everything fresh, right now" bundle so both the *original* Step 4b/4b-parity call site and the
*new* Step 4d call site are provably running the identical check:

```rust
/// Reasons the Step 4d final gate can deny a single-TX key. Distinct
/// variants only so the denial's log/diagnostic text can name the actual
/// reason -- mirrors Step 4b/4b-parity's separate messages for the same
/// two conditions, now re-evaluated at the true last instant.
enum FinalGateDenial {
    StaleQso,
    DxParityConflict,
}

/// Step 4d (PAN-148): the single-TX path's final, atomic pre-PTT gate.
/// Re-checks liveness then DX-parity, in that order (cheap sync check
/// first), using ONLY state read at THIS call -- never a value cached
/// from Step 4b/4b-parity earlier in the cycle, which can have gone stale
/// during Step 4c's pivot/remodulation or Step 5's status-announce awaits.
/// The caller MUST NOT `.await` anything else between this call returning
/// and the `SetPtt{true}` send it gates.
async fn final_single_tx_gate_denial(
    qso_id: Option<&str>,
    required_parity: pancetta_core::slot::SlotParity,
    active_tx_qsos: &Arc<RwLock<HashSet<String>>>,
    qso_manager_watch: &tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
    cross_time_state: &pancetta_qso::CrossTimeState,
    freshness_window: std::time::Duration,
) -> Option<FinalGateDenial> {
    if !tx_qso_is_live(qso_id, active_tx_qsos) {
        return Some(FinalGateDenial::StaleQso);
    }
    if dx_parity_conflict_for_qso(
        qso_id,
        required_parity,
        qso_manager_watch,
        cross_time_state,
        freshness_window,
    )
    .await
    {
        return Some(FinalGateDenial::DxParityConflict);
    }
    None
}

/// Step 4b AND Step 4d (PAN-148): per-item liveness+DX-parity mask for a
/// multi-TX bundle. Extracted so both call sites -- Step 4b's original
/// key-time gate and the new Step 4d final gate -- run the textually
/// identical check; Step 4d's caller MUST NOT `.await` anything else
/// between this call returning and the `SetPtt{true}` send it gates.
async fn bundle_live_mask(
    encoded_qso_ids: &[Option<String>],
    required_parity: pancetta_core::slot::SlotParity,
    active_tx_qsos: &Arc<RwLock<HashSet<String>>>,
    qso_manager_watch: &tokio::sync::watch::Receiver<Option<pancetta_qso::QsoManager>>,
    cross_time_state: &pancetta_qso::CrossTimeState,
    freshness_window: std::time::Duration,
) -> Vec<bool> {
    let mut parity_hold = Vec::with_capacity(encoded_qso_ids.len());
    for id in encoded_qso_ids {
        parity_hold.push(
            dx_parity_conflict_for_qso(
                id.as_deref(),
                required_parity,
                qso_manager_watch,
                cross_time_state,
                freshness_window,
            )
            .await,
        );
    }
    encoded_qso_ids
        .iter()
        .zip(parity_hold.iter())
        .map(|(id, &held)| tx_qso_is_live(id.as_deref(), active_tx_qsos) && !held)
        .collect()
}
```

Step 4b's existing inline `parity_hold_mask`/`live_mask` computation (`tx.rs:8117-8138`) is
refactored to call `bundle_live_mask` too, so there is exactly one implementation of "per-item
liveness+parity mask" in the file, called at both points in the cycle.

### Call-site changes

**Single-TX** (`tx.rs`, around the existing Step 4d-arm block, `~6255-6319`): rename the section
"Step 4d: final atomic pre-PTT gate" and, immediately before the existing remote-arm re-check
(itself unchanged), insert:

```rust
if let Some(denial) = final_single_tx_gate_denial(
    qso_id.as_deref(),
    required_parity,
    &active_tx_qsos,
    &qso_manager_watch,
    &cross_time_state,
    dx_parity_freshness_window(slot_ns),
)
.await
{
    ptt_active.store(false, Ordering::Release);
    ptt_guard.disarm();
    if let Some(key) = pivoted_this_key.take() {
        pivoted_once.remove(&key);
    }
    let (log_msg, diag_msg) = match denial {
        FinalGateDenial::StaleQso => (
            format!("dropping stale TX at final pre-PTT check for ended QSO {}: '{message_text}'", qso_id.as_deref().unwrap_or("?")),
            format!("dropping stale TX at final pre-PTT check for ended QSO: '{message_text}'"),
        ),
        FinalGateDenial::DxParityConflict => (
            format!("holding TX at final pre-PTT check for QSO {}: DX observed transmitting on the colliding parity {required_parity:?}: '{message_text}'", qso_id.as_deref().unwrap_or("?")),
            format!("holding TX at final pre-PTT check: DX observed transmitting on the colliding parity {required_parity:?}: '{message_text}'"),
        ),
    };
    info!(target: "pancetta::tx.policy", "{log_msg}");
    emit_diagnostic(&message_bus, "tx.policy", pancetta_core::DiagnosticLevel::Info, diag_msg, qso_id.as_deref()).await;
    send_tx_queue_status(&message_bus, None, Vec::new()).await;
    let complete_msg = ComponentMessage::new(/* TransmitComplete{success:false, ...} */);
    let _ = message_bus.send_message(complete_msg).await;
    continue 'worker;
}
// existing remote-arm re-check, unchanged, immediately followed by SetPtt{true}.
```

The existing remote-arm re-check runs *after* this new block, so the ordering right before
`SetPtt{true}` becomes: liveness (sync) → DX-parity (await) → arm (sync) → key. All three read
fresh state; nothing else runs between the last of them and the key.

**Multi-TX** (`tx.rs`, around the existing Step 4d-arm block, `~8643-8708`): rename the section
"Step 4d: final atomic pre-PTT gate" and, immediately before the existing remote-arm re-check,
insert a call to `bundle_live_mask` against `encoded_qso_ids_final` (the ids actually baked into
the current `audio_out`). If the result is not "every entry `true`," abort the **whole bundle**
(Option 1 above): unwind `ptt_active`/`ptt_guard`/TX-badge/queue-status exactly like the existing
arm-denial branch, remove every tombstone in `pivoted_this_bundle_keys` from `pivoted_once`, log +
diagnostic (distinguishing "ended" vs. "DX parity held" per item, matching Step 4b's existing
per-item wording), a `TransmitComplete{success: false}` for every item in `items`, `continue`. Only
when the mask is all-`true` does control fall through to the existing arm re-check and
`SetPtt{true}` — using the SAME `audio_out` Step 3 already built; no re-encode, no new await.

### Why "abort whole bundle" doesn't regress the common case

Step 4b (unchanged) still does the full rebuild-and-save-survivors work for the ~20-30s pre-PTT
sleep — the window where a QSO actually ending mid-wait is a real, expected event this file has
handled since 2026-07-17. Step 4d's new check only ever fires for something that changed in the
sub-millisecond gap between Step 4b's mask and Step 5's announce — an event Step 4b's own report
(three review rounds before anyone spotted it) shows is rare enough to have gone unnoticed through
PAN-141's entire review cycle. Trading "vanishingly rare survivor waits one more cycle" for "this
gate can never key stale/held evidence, full stop" is the correct tradeoff for a file with this
review history.

## Addendum: local review gate round 2 (Codex)

Round 1 of `codex exec review --uncommitted` found two real defects in the initial
implementation of the design above, both the same shape as the ticket itself:

1. **Single-TX:** `final_single_tx_gate_denial` checked liveness, THEN awaited the DX-parity
   lookup — a cancellation landing during that await went uncaught by this very function.
2. **Multi-TX:** `bundle_live_mask`'s per-item loop awaited `dx_parity_conflict_for_qso`
   sequentially — item 1's synchronous parity read could go stale while item 2..N's lookups
   were still pending, uncaught for the same reason.

Fix: liveness/parity reads moved to strictly AFTER the (single) await in each function.

Round 2 found a further, more subtle issue with that fix: the QsoManager lookup
(`manager.get_qso(id).await`) is a real yield point over a `tokio::sync::RwLock` that could,
under contention, suspend for an unbounded time — and it now sat AFTER Step 3's timing-sensitive
audio trim (`pad_and_cursor_for_target` against a freshly-read clock). If that await were ever
slow, `SetPtt` could fire against a cursor/pad computed for an earlier "now" than the actual key
time, misaligning the transmitted waveform — reintroducing a timing bug while fixing a liveness
one.

**Final fix:** split callsign *resolution* (the only inherently-async part — a stable fact once a
QSO is established) from the *freshness read* (`dx_parity_conflict`, over `a7_recent_calls` — a
`std::sync::RwLock`, always synchronous). `resolve_their_callsign`/`resolve_callsigns` run BEFORE
Step 3's trim in both paths (a new call site, same "await early, decide late" split PAN-72's
`qso_manager_watch` pattern already established elsewhere in this file). `final_single_tx_gate_denial`
and `bundle_live_mask` are now fully synchronous, taking the pre-resolved callsign(s) as a plain
argument — the Step 4d gate's ENTIRE body, from Step 3's trim through `SetPtt`, contains no
`.await` at all except Step 5's pre-existing, bounded, non-blocking bus-send awaits. This is a
strictly stronger guarantee than the original design: not just "no unrelated await between the
final check and the key," but "no await of any kind in that window save what was already there."

## Explicitly out of scope / invariants preserved

- Step 4b/Step 4b-parity's placement and behavior are unchanged — this ticket adds a second check,
  it does not move or remove the first one.
- `tx_parity` is never mutated (same invariant PAN-141 preserved) — both new checks are read-only.
- Never aborts an in-flight PTT — both new checks run strictly before `SetPtt{true}`, at the same
  "not yet keyed" choke point Step 4d-arm already established (PAN-140's lesson).
- `pivoted_once` bookkeeping: extended, not changed in kind — the new denial branches remove
  tombstones exactly like the existing arm-denial branches already do.
- No new config surface, no new TUI indicator — same posture as PAN-141.

## Testing plan

`tx.rs`'s own `#[cfg(test)]` modules already test this file's pre-PTT recheck primitives as
isolated functions (`dx_parity_conflict`, `tx_qso_is_live`, `multi_tx_bundle_still_fully_live`,
`emit_disarm_interrupt_signals` via `pre_ptt_denial_uses_denied_wording_not_mid_transmission_wording`)
rather than driving the full `start_transmitter_component` end-to-end — that method takes
`&mut ApplicationCoordinator` and neither `tests/coord_sim.rs` nor `tests/tx_ptt_integration.rs`
constructs a full coordinator (both are hand-written models of the gate sequence exercising the
real `MessageBus`/`MockRig`/coalescer/gate *functions*, not the literal 14k-line worker function).
This PR follows that same established pattern:

- Unit tests for `final_single_tx_gate_denial`: live+no-conflict → `None`; QSO removed from
  `active_tx_qsos` between two calls (modeling the race: a first call matching Step 4b's outcome,
  a second call after the simulated cancellation) → `Some(StaleQso)` on the second call only; a
  fresh colliding `a7_recent_calls` entry inserted between two calls → `Some(DxParityConflict)` on
  the second call only, `None` on the first — directly pinning "the SECOND, later call reflects a
  state change the first call could not have seen," which is the actual regression this ticket is
  about.
- Unit tests for `bundle_live_mask`: refactor-preserving tests asserting it's equivalent to the
  pre-refactor inline computation (all-live, all-held, mixed); a race test analogous to the
  single-TX one above (an item's QSO ends between two calls with the same `encoded_qso_ids` input
  → the second call's mask reflects it, the first does not).
- A control-flow test per path asserting the new denial branches produce a `TransmitComplete{success:
  false}` and remove any `pivoted_once` tombstone for the affected key(s) — mirroring
  `pre_ptt_denial_uses_denied_wording_not_mid_transmission_wording`'s existing style for the arm
  case.
- `cargo test --workspace --features transmit --exclude pancetta-research` and
  `cargo clippy -p pancetta -p pancetta-qso --all-targets -- -D warnings` clean, plus
  `codex exec review --uncommitted` as the local review gate before every push.
- No on-air/loopback coverage possible for a sub-millisecond race window — flagged for on-air
  sanity monitoring after merge (nothing to specifically re-verify; this is a pure hardening of an
  already-shipped gate).
