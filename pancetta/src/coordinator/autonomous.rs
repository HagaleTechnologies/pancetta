//! Autonomous operator component.
//!
//! Wires the `pancetta-qso::AutonomousOperator` decision engine into the
//! pipeline: feeds it decoded messages, lets it pick the next action
//! (call CQ, answer a CQ, ignore), and forwards the chosen TX requests
//! to the FT8 transmitter. Drives the live frequency allocator through
//! waterfall snapshots so multi-stream TX picks clear audio offsets.

use anyhow::Result;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::interval;
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};
use pancetta_core::slot::SlotParity;

/// Classify a surviving autonomous *opening* TX item into the parameters for a
/// [`crate::message_bus::QsoMessage::StartAutonomousQso`]. Kept pure (no I/O,
/// no task state) so the freq/parity-resolution logic can be unit-tested
/// without standing up the slot loop.
///
/// - A `"CQ …"` opening → `(None, our chosen offset, our tx_parity)` — we are
///   calling CQ, so we pick our own offset and parity.
/// - A pounce (`"<DX> <us> …"`) → `(Some(DX), DX's decoded freq, DX's parity)`,
///   i.e. answer Tx=Rx on the DX's frequency so its subsequent frames pass the
///   QSO relevance gate. Falls back to the item's offset and
///   `tx_parity.opposite()` (the DX parity the operator derived our TX parity
///   from) when the DX's decode for this slot can't be located.
///
/// `decodes` is this slot's decoded traffic; the DX is matched by *sender*
/// callsign (the first token of the pounce text).
pub(crate) fn classify_autonomous_opening(
    message_text: &str,
    frequency_offset: f64,
    tx_parity: Option<SlotParity>,
    decodes: &[pancetta_qso::DecodedMessageInfo],
) -> (Option<String>, f64, Option<SlotParity>) {
    let first = message_text.split_whitespace().next();
    let is_cq = first.map(|t| t.eq_ignore_ascii_case("CQ")).unwrap_or(false);
    if is_cq {
        // Calling CQ ourselves: our chosen offset + our TX parity.
        return (None, frequency_offset, tx_parity);
    }
    // Pounce: the DX is the first token. Answer on its decoded frequency.
    let dx = first.map(|s| s.to_string());
    let decoded = dx.as_ref().and_then(|d| {
        decodes.iter().find(|m| {
            m.callsign
                .as_deref()
                .map(|c| c.eq_ignore_ascii_case(d))
                .unwrap_or(false)
        })
    });
    let frequency = decoded.map(|m| m.frequency_hz).unwrap_or(frequency_offset);
    let parity = decoded
        .and_then(|m| m.slot_parity)
        .or_else(|| tx_parity.map(|p| p.opposite()));
    (dx, frequency, parity)
}

/// Resolve the cqdx.io bridge a slot tick may *publish* spots through.
///
/// `Some` only when a bridge is configured AND this process is not a
/// `--replay` demo. Replayed decodes are historical off-air captures, but
/// every `SpotReport` built from them is stamped `chrono::Utc::now()`, so
/// posting them would inject fabricated "live" spots into cqdx.io's real
/// dataset — the same failure mode the PSKReporter gate exists for (see
/// [`crate::coordinator::ApplicationCoordinator::replay_mode`]).
///
/// Read-only bridge uses (`spot_frequencies`, which feeds the frequency
/// allocator from the local cache) deliberately do NOT go through this: they
/// publish nothing, so a demo keeps showing real placement behavior.
pub(crate) fn spot_publish_target(
    bridge: Option<&std::sync::Arc<crate::cqdx_bridge::CqdxBridge>>,
    replay_mode: bool,
) -> Option<&std::sync::Arc<crate::cqdx_bridge::CqdxBridge>> {
    bridge.filter(|_| !replay_mode)
}

#[cfg(test)]
mod replay_gate_tests {
    use super::*;
    use crate::cqdx_bridge::CqdxBridge;
    use crate::priority_evaluator::CachedStationLookup;
    use std::sync::Arc;

    /// A real (never-used) bridge: `from_config` only builds a reqwest
    /// client, so this touches no network. The token has to satisfy
    /// `CqdxClient::new`'s PAT format check (`pat_` prefix, >= 16 chars).
    fn bridge() -> Arc<CqdxBridge> {
        let cfg = pancetta_config::network::CqdxConfig {
            enabled: true,
            token: Some("pat_0123456789abcdef".to_string()),
            ..Default::default()
        };
        Arc::new(
            CqdxBridge::from_config(&cfg, Arc::new(CachedStationLookup::new()))
                .expect("enabled + non-empty token yields a bridge"),
        )
    }

    #[tokio::test]
    async fn configured_bridge_publishes_spots_when_not_replaying() {
        let b = bridge();
        assert!(
            spot_publish_target(Some(&b), false).is_some(),
            "a live run with cqdx.io configured must still report spots"
        );
    }

    #[tokio::test]
    async fn replay_suppresses_spot_publishing_even_with_a_configured_bridge() {
        let b = bridge();
        assert!(
            spot_publish_target(Some(&b), true).is_none(),
            "--replay must never POST replayed decodes to cqdx.io as live spots"
        );
    }

    #[tokio::test]
    async fn no_bridge_is_none_either_way() {
        assert!(spot_publish_target(None, false).is_none());
        assert!(spot_publish_target(None, true).is_none());
    }
}

#[cfg(test)]
mod pan6_diagnostic_tests {
    use super::*;

    fn plan(runtime: usize, policy: usize, presence: usize) -> SlotPlan {
        SlotPlan {
            runtime_gate_dropped: runtime,
            policy_dropped: policy,
            presence_dropped: presence,
            ..SlotPlan::default()
        }
    }

    #[test]
    fn gate_diagnostics_are_edge_triggered() {
        let mut state = GateDiagState::default();
        assert_eq!(
            gate_diagnostics_for_slot(
                &mut state,
                &plan(2, 0, 0),
                false,
                pancetta_core::TxPolicy::Full,
                true
            )
            .len(),
            1
        );
        assert!(gate_diagnostics_for_slot(
            &mut state,
            &SlotPlan::default(),
            false,
            pancetta_core::TxPolicy::Full,
            true,
        )
        .is_empty());
        let resumed = gate_diagnostics_for_slot(
            &mut state,
            &plan(0, 0, 0),
            true,
            pancetta_core::TxPolicy::Full,
            true,
        );
        assert_eq!(resumed.len(), 1);
        assert!(resumed[0].text.contains("resumed"));
    }

    #[test]
    fn a_policy_change_while_still_suppressing_re_emits() {
        let mut state = GateDiagState::default();
        let first = gate_diagnostics_for_slot(
            &mut state,
            &plan(0, 1, 0),
            true,
            pancetta_core::TxPolicy::RespondOnly,
            true,
        );
        let changed = gate_diagnostics_for_slot(
            &mut state,
            &plan(0, 1, 0),
            true,
            pancetta_core::TxPolicy::Disabled,
            true,
        );
        assert_eq!(first.len(), 1);
        assert_eq!(changed.len(), 1);
        assert!(changed[0].text.contains("DISABLED"));
    }

    #[test]
    fn policy_suppression_takes_precedence_over_presence() {
        let mut state = GateDiagState::default();
        let diagnostics = gate_diagnostics_for_slot(
            &mut state,
            &plan(0, 1, 1),
            true,
            pancetta_core::TxPolicy::RespondOnly,
            false,
        );
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics.iter().any(|d| d.text.contains("TX policy")));
        assert!(!diagnostics.iter().any(|d| d.text.contains("presence gate")));
    }

    #[test]
    fn a_quiet_slot_emits_nothing() {
        let mut state = GateDiagState::default();
        assert!(gate_diagnostics_for_slot(
            &mut state,
            &SlotPlan::default(),
            true,
            pancetta_core::TxPolicy::Full,
            true,
        )
        .is_empty());
    }

    #[test]
    fn gate_text_does_not_collide_with_tui_drop_counter_prefix() {
        let mut state = GateDiagState::default();
        let diagnostics = gate_diagnostics_for_slot(
            &mut state,
            &plan(1, 1, 1),
            false,
            pancetta_core::TxPolicy::Disabled,
            false,
        );
        assert!(diagnostics
            .iter()
            .all(|d| !d.text.starts_with("dropping stale")));
    }

    #[test]
    fn callsign_continuity_diagnostic_includes_the_score() {
        let diagnostic = skip_record_diagnostic(&pancetta_qso::CqSkipRecord {
            callsign: Some("W1AW".into()),
            reason: pancetta_qso::SkipReason::CallsignContinuity { dx_score: 0.42 },
        });
        assert_eq!(diagnostic.target, "qso.security");
        assert!(diagnostic.text.contains("W1AW"));
        assert!(diagnostic.text.contains("score 0.42"));
    }

    #[test]
    fn skip_suppression_is_per_reason_and_callsign_and_bounded() {
        let mut seen = SkipDiagSeen::default();
        let now = std::time::Instant::now();
        assert!(should_report_skip(
            &mut seen,
            "dx_busy",
            Some("JA1ABC"),
            now
        ));
        assert!(!should_report_skip(
            &mut seen,
            "dx_busy",
            Some("JA1ABC"),
            now
        ));
        assert!(!should_report_skip(
            &mut seen,
            "dx_busy",
            Some("JA1ABC"),
            now + Duration::from_secs(60)
        ));
        assert!(should_report_skip(
            &mut seen,
            "dx_busy",
            Some("JA1ABC"),
            now + SKIP_DIAG_REPEAT_WINDOW + Duration::from_secs(1)
        ));
        assert!(should_report_skip(
            &mut seen,
            "dx_busy",
            Some("VK2XYZ"),
            now
        ));
        assert!(should_report_skip(&mut seen, "at_capacity", None, now));
        for i in 0..2_000 {
            let call = format!("X{i}");
            should_report_skip(&mut seen, "dx_busy", Some(&call), now);
        }
        assert!(seen.len() <= 1024);
    }
}

/// Maps the operator's parked TX offset (Hz) to the openness code (0-3) at
/// that bin in a [`pancetta_qso::frequency::PlacementSnapshot`] — the
/// coordinator-side counterpart to the TUI's own parked-bin lookup (Task
/// 14's `apply_placement`), used to drive the persistent
/// `DiagnosticEvent` warning below.
///
/// Returns `None` when `parked_hz == 0` (the `tx_offset_hold_hz` unset
/// sentinel — matches the convention used elsewhere, e.g.
/// `coordinator/qso.rs`'s `compute_manual_tx_offset`) or when the parked
/// frequency falls outside the snapshot's range.
///
/// Uses the shared `pancetta_core::freq_bin::bin_index_for_freq` — the
/// TUI's own parked-bin lookup (`pancetta-tui`'s `bin_index_for_freq`,
/// Task 14's `apply_placement`) calls the same function, so the two sides
/// can no longer disagree at the range's top edge (issue #97 item 3).
fn parked_bin_coverage(
    parked_hz: u64,
    snap: &pancetta_qso::frequency::PlacementSnapshot,
) -> Option<u8> {
    if parked_hz == 0 {
        return None;
    }
    let idx = pancetta_core::freq_bin::bin_index_for_freq(
        parked_hz as f64,
        snap.range,
        snap.bin_hz,
        snap.openness.len(),
    )?;
    snap.openness.get(idx).copied()
}

/// Whole-branch-review fix (finding 2): resolves the active-QSO count fed
/// into [`should_repark`]'s LIVE STREAM SAFETY gate, failing **CLOSED** on a
/// poisoned `active_tx_qsos` lock — i.e. a lock-read error is treated as
/// "assume a QSO may be active" (`usize::MAX`, so `should_repark`'s
/// `active_qsos > 0` check trips and the repark is skipped this tick) rather
/// than "assume zero" (which would let the gate happily proceed as if idle
/// even though the lock state is unknown). This is the opposite of
/// `active_now`'s `.unwrap_or(0)` a few lines above in the call site, which
/// intentionally fails OPEN — that read feeds only the operator's
/// `max_concurrent_qsos` display/gating count, a different, non-safety
/// concern (the engine's own dedup/in-progress gates still apply there).
/// Matches the codebase's established fail-closed convention for
/// TX-adjacent gates under lock/state uncertainty (the remote-TX arm gate,
/// `coordinator/tx.rs`). Kept as a tiny pure function over an
/// already-resolved `Result` (rather than inlined at the call site) so the
/// fail-closed behavior is directly unit-testable without spinning up the
/// autonomous task.
fn resolve_repark_active_qsos<E>(active_qsos_read: Result<usize, E>) -> usize {
    active_qsos_read.unwrap_or(usize::MAX)
}

/// Opt-in auto-repark decision (Task 16). Repark ONLY when: `enabled` ∧ no
/// active QSOs ∧ currently parked ∧ the parked bin is busy-both (openness
/// code 0) ∧ the best available slice beats the parked slice's CURRENT
/// score by ≥ `min_gain`. Returns the new offset (Hz) to park at, or `None`
/// to hold.
///
/// **Live-stream safety**: `active_qsos > 0` short-circuits to `None`
/// unconditionally, before any other check. This function only ever
/// *decides*; the caller is responsible for re-checking `active_tx_qsos`
/// at write-time (see the loop wiring below) so the gate can never fire
/// against a state that went live between the decision and the write.
///
/// **Hysteresis**: reparking only fires out of the worst openness code (0 =
/// busy-both). Any other code (1/2/3 — at least one slot still clear) holds,
/// so a marginally-degraded-but-still-usable parked slice is left alone.
/// The `min_gain` threshold on top of that prevents chasing a trivially
/// better slice.
fn should_repark(
    enabled: bool,
    active_qsos: usize,
    parked_hz: u64,
    parked_coverage: Option<u8>,
    parked_score: Option<f64>,
    best: Option<&pancetta_qso::frequency::FrequencyCandidate>,
    min_gain: f64,
) -> Option<u64> {
    if !enabled {
        return None;
    }
    // LIVE STREAM SAFETY: never repark while any QSO is active. This is the
    // one hard gate in this function; nothing below can override it.
    if active_qsos > 0 {
        return None;
    }
    if parked_hz == 0 {
        // Not currently parked — nothing to repark.
        return None;
    }
    // Hysteresis: only repark out of the worst openness code (busy-both).
    let coverage = parked_coverage?;
    if coverage != 0 {
        return None;
    }
    let best = best?;
    // If the parked offset fell out of the top-N snapshot entirely, treat
    // its current score as 0.0 (worst case) rather than skipping the
    // decision — an untracked parked bin under busy-both coverage is, by
    // definition, not a good place to stay parked.
    let parked_score = parked_score.unwrap_or(0.0);
    if best.score - parked_score >= min_gain {
        Some(best.offset_hz as u64)
    } else {
        None
    }
}

/// Looks up the parked offset's CURRENT score in a
/// [`pancetta_qso::frequency::PlacementSnapshot`]'s top-N `slices` (the
/// NEAREST candidate whose `offset_hz` falls within half a bin width of
/// `parked_hz`), for feeding [`should_repark`]'s `parked_score` parameter.
///
/// **Documented ambiguity resolution (Task 16 brief):** the snapshot only
/// carries the top-N ranked candidates, so the parked bin may not be among
/// them — either it never scored well enough, or it degraded out of the
/// top-N this tick. Re-deriving a fresh score for that one bin would mean a
/// SECOND allocator/scorer invocation, breaking the single-scorer invariant
/// this instrument holds everywhere else (every other consumer reads the
/// SAME `placement_snapshot` the autonomous decision engine itself used).
/// So an absent parked bin resolves to `None` here — the caller
/// (`should_repark`'s call site) maps that to a worst-case `Some(0.0)`,
/// i.e. "no evidence this is a good slice to stay on," rather than
/// re-scoring or skipping the repark decision entirely.
///
/// Uses `min_by` over ALL candidates within the half-bin-width window
/// (issue #98 item 2) rather than the first one encountered — today the
/// allocator guarantees at most one candidate per `bin_hz`-spaced bin, so
/// the two are equivalent in practice, but nearest-by-distance doesn't rely
/// on that as an implicit, unenforced invariant.
fn parked_score_in_slices(
    parked_hz: u64,
    snap: &pancetta_qso::frequency::PlacementSnapshot,
) -> Option<f64> {
    if parked_hz == 0 {
        return None;
    }
    let parked = parked_hz as f64;
    snap.slices
        .iter()
        .filter(|c| (c.offset_hz - parked).abs() <= snap.bin_hz / 2.0)
        .min_by(|a, b| {
            (a.offset_hz - parked)
                .abs()
                .total_cmp(&(b.offset_hz - parked).abs())
        })
        .map(|c| c.score)
}

/// Parameters for opening one autonomous QSO (a resolved
/// [`crate::message_bus::QsoMessage::StartAutonomousQso`]).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AutonomousQsoStart {
    pub callsign: Option<String>,
    pub frequency: f64,
    pub parity: Option<SlotParity>,
    /// PAN-38: `Some(id)` only for a self-CQ (`callsign: None`) — the
    /// `AutonomousOperator`'s `CqStateSnapshot::attempt_id` this opening's
    /// speculative streak/offset mutations belong to. `None` for a pounce
    /// (no CQ-streak state to roll back) or when `decide_at` didn't record a
    /// snapshot for this cycle.
    pub cq_attempt_id: Option<u64>,
}

/// The fully-gated, routed result of one autonomous decision slot: which QSOs
/// to open in the `QsoManager` and which raw `TransmitRequest`s to bundle.
#[derive(Debug, Default)]
pub(crate) struct SlotPlan {
    /// Openings to create as Auto QSOs (the `QsoManager` emits their TX).
    pub qso_starts: Vec<AutonomousQsoStart>,
    /// Items to transmit raw (QSO-in-progress sequencer items, qso_id=Some).
    pub tx_items: Vec<(crate::message_bus::TransmitRequestItem, Option<SlotParity>)>,
    /// Message texts of openings that were *not* opened because `dry_run` is on
    /// (for operator-facing logging only).
    pub dry_run_openings: Vec<String>,
    /// How many items the Shift+Q runtime gate dropped.
    pub runtime_gate_dropped: usize,
    /// How many initiation items the TX policy suppressed.
    pub policy_dropped: usize,
    /// How many initiation items the operator-presence gate suppressed (policy
    /// allowed initiation, but no recent console activity — FCC §97.221).
    pub presence_dropped: usize,
}

/// Pure decision: turn one slot's collected TX items into a [`SlotPlan`],
/// applying — in order — (1) the Shift+Q runtime gate (drops everything when
/// closed), (2) the tri-state TX-policy initiation suppression (drops
/// `qso_id == None` items unless the policy allows initiation), and (3) the
/// opening→QSO-start split (each surviving `qso_id == None` opening becomes an
/// `AutonomousQsoStart` — routed through the `QsoManager` instead of a raw TX,
/// so no double-send — while `qso_id == Some` items stay on the raw TX path).
/// `dry_run` records openings without opening them.
///
/// Codex review (PR #276): `AutonomousOperator::decide_at` counts a self-CQ
/// toward its no-response streak as soon as it emits the `Transmit` action —
/// before any of the gates `plan_slot_transmissions` applies have run. This
/// mirrors those same gating conditions (extracted as its own pure function,
/// same rationale as `plan_slot_transmissions`, so it can never drift out of
/// sync with what actually got suppressed) to tell the caller whether THIS
/// cycle's self-CQ needs its optimistic increment undone via
/// `AutonomousOperator::discount_suppressed_cq`.
///
/// `self_cq_emitted` is `true` iff `decide_at`'s actions this cycle included
/// a self-CQ (`Transmit` with `qso_id: None` and CQ-shaped text — distinct
/// from a pounce opening, which doesn't affect the no-response streak).
/// Drain the queue-independent self-CQ dispatch-failure fallback
/// (`ApplicationCoordinator::pending_autonomous_cq_dispatch_failures`) into
/// real rollbacks against the live operator.
///
/// PAN-43/45 follow-up (Codex round 2 on PR #342): the QSO task falls back
/// to pushing an `attempt_id` onto this shared list once its own bounded
/// `AutonomousCqDispatchFailed` message-bus retries are exhausted --
/// mirroring `hamlib_pending_frequency`'s (PAN-19 round 10) shared-state
/// pattern for the identical class of problem. This is the other half:
/// called once per `slot_interval` tick, entirely independent of this
/// task's own message-bus channel, so it drains correctly no matter how
/// congested that channel currently is. Behavior is intentionally
/// identical to the message-driven `AutonomousCqDispatchFailed` handler in
/// the same task's `select!` loop -- same two calls, same order.
fn drain_pending_autonomous_cq_dispatch_failures(
    op: &mut pancetta_qso::AutonomousOperator,
    pending_self_cq_qsos: &mut std::collections::HashMap<String, u64>,
    pending_autonomous_cq_dispatch_failures: &std::sync::Mutex<Vec<u64>>,
    rolled_back_attempt_tombstones: &mut std::collections::VecDeque<u64>,
) {
    let failed_attempts: Vec<u64> = std::mem::take(
        &mut *pending_autonomous_cq_dispatch_failures
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    for attempt_id in failed_attempts {
        op.restore_cq_state_for_attempt(attempt_id);
        pending_self_cq_qsos.retain(|_, id| *id != attempt_id);
        record_rolled_back_attempt_tombstone(rolled_back_attempt_tombstones, attempt_id);
    }
}

/// PAN-72: resolves and commits every queued mid-QSO TX-offset action once
/// per tick, mirroring `drain_pending_autonomous_cq_dispatch_failures`'s
/// mailbox shape. `Switch` is resolved via the SAME `allocate_smart_frequency`
/// the CQ-hunting switch uses (single-scorer invariant); `Revert` needs no
/// allocator call. Errors from `apply_tx_offset_switch` are logged and
/// skipped, not propagated — a request for a QSO that has since completed,
/// gone terminal or been superseded by a DX advance is an expected race
/// (`QsoManagerError::is_expected_offset_action_refusal`, logged at `debug!`),
/// not this task's problem to recover from.
///
/// **Hold is re-read here, per request** (Codex round 1 on PR #350, finding
/// 6). A stall or `u` nudge can be queued while the mode is Auto and the
/// operator can then press `f` for Hold before this once-per-slot drain
/// runs. Committing anyway would violate Hold's promise that autonomous
/// offset changes are suppressed — and in two different ways: a queued
/// `Switch` resolves through `allocate_smart_frequency`'s Hold early return
/// (which ignores `avoid_hz` and hands back the *parked* offset, dragging a
/// held QSO onto it), while a queued `Revert` never consults the allocator at
/// all and would move straight to its target. Queued actions are discarded
/// wholesale once the mode reads Hold. This mirrors the same fail-safe the
/// TUI's `u` handler applies at enqueue time; both ends need it, because the
/// mode can flip in between.
///
/// **Each committed offset is reserved for the rest of the batch** (finding
/// 1). This operator's own-frequency snapshot is only synced from
/// `active_tx_offsets` later in the same tick (`set_own_frequencies`, a few
/// statements below this drain's call site), so every `Switch` in one batch
/// would otherwise rank against the identical stale occupancy view, each
/// excluding only its own original offset — and two concurrent QSOs could
/// land on the same "best" candidate, collapsing two TX streams onto one
/// frequency. `reserved_hz` accumulates every offset this batch has actually
/// committed and feeds it to `allocate_smart_frequency_avoiding` as an extra
/// hard exclusion for the remaining requests. `Revert` targets are reserved
/// too: a revert commits an offset just as surely as a switch does. Currently
/// bounded in practice by `max_concurrent_qsos` (default 1), but it is a real
/// bug the moment that is raised.
///
/// Caveat: this reservation only binds when `allocate_smart_frequency_avoiding`
/// takes its spectral-snapshot branch — with no spectral snapshot yet it falls
/// through to the deterministic legacy allocator, which ignores `reserved_hz`
/// entirely, so two same-batch `Switch` actions could still collide in that
/// fallback case (see that function's own doc comment).
///
/// On success the QSO's entry in `active_tx_offsets` is rewritten to the
/// offset `apply_tx_offset_switch` actually applied (post-clamp). That map is
/// otherwise refreshed only on a `QsoEvent::StateChanged` carrying a
/// frequency. `apply_tx_offset_switch` does emit `QsoEvent::TxOffsetApplied`
/// on success, but purely to trigger a UI-snapshot refresh (the event carries
/// no state transition and its handler never touches this map) — so without
/// this direct write the map would still keep the PRE-switch offset until the
/// QSO's next state transition. Two consumers read it: the allocator, every
/// tick, via `set_own_frequencies` (a few statements below this drain's call
/// site — so the mirror lands in time for the same tick's sync), and the `u`
/// nudge, which builds its `avoid_hz` from it. A stale entry therefore both guards a
/// slot the QSO has already vacated — leaving the allocator free to hand it to
/// something else — and leaves the allocator blind to the offset the QSO is
/// really on, while the next nudge would name the wrong frequency to avoid.
///
/// A direct map write (rather than a new event type) is the right shape
/// here: `active_tx_offsets` is a plain `Arc<RwLock<HashMap<String, f64>>>`
/// already written from several places in `coordinator/qso.rs`, and the key
/// format is the same `active_tx_qso_key` the event-forwarder uses. Only
/// entries that already exist are updated — an absent key means the
/// forwarder does not consider this QSO TX-active, and resurrecting it here
/// would re-admit a QSO the `Failed`/completion purge just removed.
async fn drain_pending_qso_offset_requests(
    op: &mut pancetta_qso::AutonomousOperator,
    qso_manager: &pancetta_qso::QsoManager,
    pending_qso_offset_requests: &std::sync::Mutex<
        Vec<pancetta_qso::qso_manager::OffsetActionRequest>,
    >,
    active_tx_offsets: &std::sync::RwLock<std::collections::HashMap<String, f64>>,
    tx_freq_mode: &std::sync::atomic::AtomicU8,
) {
    let requests: Vec<_> = std::mem::take(
        &mut *pending_qso_offset_requests
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()),
    );
    if requests.is_empty() {
        return;
    }
    // Finding 6: authoritative re-read at commit time, not at enqueue time.
    if !pancetta_core::TxFreqMode::from_u8(tx_freq_mode.load(Ordering::Acquire))
        .allows_auto_change()
    {
        info!(
            target: "tx.freq",
            discarded = requests.len(),
            "PAN-72: TX-frequency mode is Hold — discarding queued TX-offset actions"
        );
        return;
    }
    // Finding 1: offsets this batch has already committed, hard-excluded from
    // every subsequent resolution so two QSOs can't collapse onto one slot.
    let mut reserved_hz: Vec<f64> = Vec::new();
    for request in requests {
        let pancetta_qso::qso_manager::OffsetActionRequest {
            qso_id,
            action,
            raised_at_generation,
        } = request;
        let resolved_hz = match action {
            pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz } => {
                op.allocate_smart_frequency_avoiding(None, None, Some(avoid_hz), &reserved_hz)
            }
            pancetta_qso::qso_manager::OffsetAction::Revert { target_hz } => target_hz,
        };
        match qso_manager
            .apply_tx_offset_switch(qso_id, resolved_hz, raised_at_generation)
            .await
        {
            Ok(applied_hz) => {
                // Reserve only what actually landed (post-clamp) — and only
                // once the commit succeeded, so a refused request never
                // sterilizes a slot nothing is using.
                reserved_hz.push(applied_hz);
                let key = super::active_tx_qso_key(&qso_id.to_string());
                if let Ok(mut offsets) = active_tx_offsets.write() {
                    if let Some(slot) = offsets.get_mut(&key) {
                        *slot = applied_hz;
                    }
                }
            }
            Err(err) if err.is_expected_offset_action_refusal() => {
                // The QSO completed, went terminal, or advanced between the
                // action being raised and this once-per-slot drain. All three
                // are ordinary races, not faults.
                debug!(
                    target: "tx.freq",
                    qso_id = %qso_id,
                    error = %err,
                    "PAN-72: queued TX-offset action superseded — discarded"
                );
            }
            Err(err) => {
                warn!(
                    target: "tx.freq",
                    qso_id = %qso_id,
                    error = %err,
                    "PAN-72: could not apply queued TX-offset action"
                );
            }
        }
    }
}

/// Bound on `rolled_back_attempt_tombstones` -- self-CQ attempts run at
/// most one or two at a time in practice, so this is a defensive cap
/// against pathological growth (a fallback-drained attempt whose
/// registration was declined outright, PAN-45, never has a matching
/// `AutonomousCqOpened` to consume its tombstone at all -- it would
/// otherwise sit forever), not a realistic operating ceiling.
const MAX_ROLLED_BACK_ATTEMPT_TOMBSTONES: usize = 16;

/// Record that `attempt_id` was just rolled back, so a same-attempt
/// `AutonomousCqOpened` that's still queued on the message bus (and gets
/// processed AFTER this rollback, per the race `drain_pending_autonomous_
/// cq_dispatch_failures`'s doc comment describes) can recognize it's
/// already-dead and skip inserting into `pending_self_cq_qsos` -- see
/// [`consume_rolled_back_attempt_tombstone`].
fn record_rolled_back_attempt_tombstone(
    tombstones: &mut std::collections::VecDeque<u64>,
    attempt_id: u64,
) {
    if tombstones.contains(&attempt_id) {
        return;
    }
    tombstones.push_back(attempt_id);
    if tombstones.len() > MAX_ROLLED_BACK_ATTEMPT_TOMBSTONES {
        tombstones.pop_front();
    }
}

/// Check whether `attempt_id` was already rolled back (see
/// [`record_rolled_back_attempt_tombstone`]), consuming the tombstone if
/// so -- single-use, since a given `attempt_id` is never reused.
fn consume_rolled_back_attempt_tombstone(
    tombstones: &mut std::collections::VecDeque<u64>,
    attempt_id: u64,
) -> bool {
    if let Some(pos) = tombstones.iter().position(|&id| id == attempt_id) {
        tombstones.remove(pos);
        true
    } else {
        false
    }
}

pub(crate) fn self_cq_suppressed(
    self_cq_emitted: bool,
    runtime_gate_open: bool,
    policy: pancetta_core::TxPolicy,
    operator_present: bool,
    dry_run: bool,
) -> bool {
    if !self_cq_emitted {
        return false;
    }
    // (1) Shift+Q runtime gate closed: drops everything, including the CQ.
    // (2) Policy+presence: suppress `qso_id: None` initiations specifically
    //     — a self-CQ always has `qso_id: None`, so this always applies to
    //     it when initiation isn't allowed.
    // (3) dry_run: opening items are diverted to `dry_run_openings` instead
    //     of `qso_starts` — never forwarded to the transmitter either.
    let initiation_allowed = policy.allows_initiation() && operator_present;
    !runtime_gate_open || !initiation_allowed || dry_run
}

/// Extracted as a pure function so the full gating/routing matrix is unit
/// testable without the wall-clock slot loop. The spawned task only does I/O
/// (logging + `send_message`) around this.
pub(crate) fn plan_slot_transmissions(
    mut tx_items: Vec<(crate::message_bus::TransmitRequestItem, Option<SlotParity>)>,
    runtime_gate_open: bool,
    policy: pancetta_core::TxPolicy,
    dry_run: bool,
    listen_messages: &[pancetta_qso::DecodedMessageInfo],
    operator_present: bool,
    // PAN-38: `AutonomousOperator::last_cq_attempt_id()`, captured by the
    // caller right after `decide()` — attached to the self-CQ opening (if
    // one survives every gate below) so a downstream dispatch failure can
    // be traced back to this attempt's speculative streak/offset mutations.
    self_cq_attempt_id: Option<u64>,
) -> SlotPlan {
    // (1) Shift+Q runtime gate: closed → drop everything this cycle.
    let mut runtime_gate_dropped = 0;
    if !runtime_gate_open && !tx_items.is_empty() {
        runtime_gate_dropped = tx_items.len();
        tx_items.clear();
    }

    // (2) Suppress autonomous *initiations* (qso_id == None: CQ-self + pounce)
    // unless BOTH the TX policy allows initiation AND the operator is present
    // (recent console activity). QSO-in-progress items (qso_id == Some) always
    // flow. The presence gate is the FCC §97.221 control: an unattended station
    // (automatic control) must not ORIGINATE contact on the standard FT8
    // frequencies; it may only continue/respond. "Present" = the operator did
    // something at the console within the presence window (see the autonomous
    // dispatch). Headless / idle → not present → respond-only initiation.
    let initiation_allowed = policy.allows_initiation() && operator_present;
    let mut policy_dropped = 0;
    let mut presence_dropped = 0;
    if !initiation_allowed {
        let before = tx_items.len();
        tx_items.retain(|(item, _)| item.qso_id.is_some());
        let dropped = before - tx_items.len();
        // Attribute the drop to its reason for the operator-facing logs.
        if !policy.allows_initiation() {
            policy_dropped = dropped;
        } else {
            presence_dropped = dropped;
        }
    }

    // (3) Opening → QSO-start split.
    let mut qso_starts = Vec::new();
    let mut dry_run_openings = Vec::new();
    let mut remaining = Vec::with_capacity(tx_items.len());
    for (item, tx_parity) in tx_items.into_iter() {
        if item.qso_id.is_some() {
            remaining.push((item, tx_parity));
            continue;
        }
        if dry_run {
            dry_run_openings.push(item.message_text.clone());
            continue;
        }
        let (callsign, frequency, parity) = classify_autonomous_opening(
            &item.message_text,
            item.frequency_offset,
            tx_parity,
            listen_messages,
        );
        // `classify_autonomous_opening` returns `callsign: None` only for a
        // CQ-shaped opening (self-CQ) — a pounce always carries `Some(dx)`.
        let cq_attempt_id = if callsign.is_none() {
            self_cq_attempt_id
        } else {
            None
        };
        qso_starts.push(AutonomousQsoStart {
            callsign,
            frequency,
            parity,
            cq_attempt_id,
        });
    }

    SlotPlan {
        qso_starts,
        tx_items: remaining,
        dry_run_openings,
        runtime_gate_dropped,
        policy_dropped,
        presence_dropped,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateDiagnostic {
    pub target: &'static str,
    pub level: pancetta_core::DiagnosticLevel,
    pub text: String,
}

#[derive(Debug, Default)]
pub(crate) struct GateDiagState {
    runtime_gate_suppressing: bool,
    policy_suppressing: Option<pancetta_core::TxPolicy>,
    presence_suppressing: bool,
}

pub(crate) fn gate_diagnostics_for_slot(
    state: &mut GateDiagState,
    plan: &SlotPlan,
    runtime_gate_open: bool,
    policy: pancetta_core::TxPolicy,
    operator_present: bool,
) -> Vec<GateDiagnostic> {
    use pancetta_core::DiagnosticLevel as L;
    let mut out = Vec::new();
    let runtime_now = !runtime_gate_open;
    if runtime_now && !state.runtime_gate_suppressing {
        out.push(GateDiagnostic { target: "operator.override", level: L::Warn, text: format!("Autonomous runtime gate OFF (Shift+Q) — dropping autonomous TX; {} item(s) this slot", plan.runtime_gate_dropped) });
    } else if !runtime_now && state.runtime_gate_suppressing {
        out.push(GateDiagnostic {
            target: "operator.override",
            level: L::Info,
            text: "Autonomous runtime gate ON — autonomous TX resumed".into(),
        });
    }
    state.runtime_gate_suppressing = runtime_now;
    let policy_now = (!policy.allows_initiation()).then_some(policy);
    if policy_now.is_some() && policy_now != state.policy_suppressing {
        out.push(GateDiagnostic {
            target: "tx.policy",
            level: L::Info,
            text: format!(
                "TX policy {} — suppressing autonomous initiation; QSO-in-progress items still TX",
                policy.label()
            ),
        });
    } else if policy_now.is_none() && state.policy_suppressing.is_some() {
        out.push(GateDiagnostic {
            target: "tx.policy",
            level: L::Info,
            text: "TX policy allows initiation — autonomous initiation resumed".into(),
        });
    }
    state.policy_suppressing = policy_now;
    let presence_now = policy.allows_initiation() && !operator_present;
    if presence_now && !state.presence_suppressing {
        out.push(GateDiagnostic { target: "tx.policy", level: L::Info, text: format!("Operator-presence gate (FCC §97.221) — no console activity in {}s; suppressing autonomous initiation. Press any key to resume", super::OPERATOR_PRESENCE_WINDOW.as_secs()) });
    } else if !presence_now && state.presence_suppressing {
        out.push(GateDiagnostic {
            target: "tx.policy",
            level: L::Info,
            text: "Operator present — autonomous initiation resumed".into(),
        });
    }
    state.presence_suppressing = presence_now;
    out
}

pub(crate) const SKIP_DIAG_REPEAT_WINDOW: std::time::Duration = std::time::Duration::from_secs(300);

#[derive(Debug, Default)]
pub(crate) struct SkipDiagSeen(
    std::collections::HashMap<(&'static str, String), std::time::Instant>,
);

impl SkipDiagSeen {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.0.len()
    }
}

pub(crate) fn should_report_skip(
    seen: &mut SkipDiagSeen,
    reason_key: &'static str,
    callsign: Option<&str>,
    now: std::time::Instant,
) -> bool {
    seen.0
        .retain(|_, when| now.saturating_duration_since(*when) <= SKIP_DIAG_REPEAT_WINDOW);
    let key = (reason_key, callsign.unwrap_or_default().to_string());
    if seen.0.contains_key(&key) {
        return false;
    }
    if seen.0.len() >= 1024 {
        if let Some(oldest) = seen
            .0
            .iter()
            .min_by_key(|(_, when)| **when)
            .map(|(key, _)| key.clone())
        {
            seen.0.remove(&oldest);
        }
    }
    seen.0.insert(key, now);
    true
}

fn skip_reason_key(reason: &pancetta_qso::SkipReason) -> &'static str {
    match reason {
        pancetta_qso::SkipReason::AtCapacity { .. } => "at_capacity",
        pancetta_qso::SkipReason::DxBusy { .. } => "dx_busy",
        pancetta_qso::SkipReason::RecentlyResponded => "recently_responded",
        pancetta_qso::SkipReason::CallsignContinuity { .. } => "callsign_continuity",
        pancetta_qso::SkipReason::ContentScore { .. } => "content_score",
        pancetta_qso::SkipReason::FrequencyClash => "frequency_clash",
    }
}

pub(crate) fn skip_record_diagnostic(record: &pancetta_qso::CqSkipRecord) -> GateDiagnostic {
    use pancetta_core::DiagnosticLevel as L;
    let call = record.callsign.as_deref().unwrap_or("unknown");
    match record.reason {
        pancetta_qso::SkipReason::AtCapacity { active, cap } => GateDiagnostic {
            target: "tx.policy",
            level: L::Info,
            text: format!("Not calling any CQ — at capacity {active}/{cap}"),
        },
        pancetta_qso::SkipReason::DxBusy { window_secs } => GateDiagnostic {
            target: "qso.autonomous",
            level: L::Info,
            text: format!(
                "Skipped CQ from {call} — DX working a third party (last {window_secs}s)"
            ),
        },
        pancetta_qso::SkipReason::RecentlyResponded => GateDiagnostic {
            target: "qso.autonomous",
            level: L::Info,
            text: format!("Skipped CQ from {call} — already answered within the last 60s"),
        },
        pancetta_qso::SkipReason::CallsignContinuity { dx_score } => GateDiagnostic {
            target: "qso.security",
            level: L::Warn,
            text: format!(
                "Rejected CQ from {call} — callsign not in the trust set (score {dx_score:.2})"
            ),
        },
        pancetta_qso::SkipReason::ContentScore { score, threshold } => GateDiagnostic {
            target: "qso.security",
            level: L::Warn,
            text: format!("Rejected CQ from {call} — content score {score:.2} < {threshold:.2}"),
        },
        pancetta_qso::SkipReason::FrequencyClash => GateDiagnostic {
            target: "qso.autonomous",
            level: L::Info,
            text: format!("Skipped CQ from {call} — frequency clashes with our own TX"),
        },
    }
}

impl super::ApplicationCoordinator {
    pub(crate) async fn start_autonomous_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_autonomous");
        let _enter = span.enter();

        let config = self.config.read().await;
        let auto_config_enabled = config.autonomous.enabled;

        // hb-161 + 2026-07-29 follow-up: seed the runtime gate. An
        // interactive (TUI) launch always starts CLOSED regardless of
        // config — the operator must press `a` to arm, the same
        // safety-driver posture as Shift+Q's disarm. Auto-arming straight
        // from config is reserved for `--headless` (`docs/RUNBOOK.md`:
        // "for a supervised headless station, config is the only
        // switch" — there is no TUI to press `a` in, so config must be
        // able to arm it directly there). If the operator launched with
        // autonomous=false in config, the gate is already `false` either
        // way and any `a`-press is a no-op (idempotent — the desired
        // safety-driver property).
        let seed_armed = auto_config_enabled && self.headless;
        self.autonomous_enabled_runtime
            .store(seed_armed, Ordering::Release);

        // FQ-F9: `auto_config_enabled` no longer early-returns into a
        // no-op drain task. The slot-tick loop below always runs — it
        // drives the TX-placement instrument (spectral snapshot,
        // decode-history feed, `TxPlacementUpdate`) for every operator,
        // manual-only included, since the 2026-07-03 TUI redesign made
        // that instrument the primary spectrum/openness view for ALL
        // operators, not just autonomous users. Only the decision-making
        // (`op.decide()`) and TX/action dispatch downstream of it are
        // gated on `auto_config_enabled` — see the slot-tick loop body.
        if auto_config_enabled {
            info!("Starting autonomous operator component");
        } else {
            info!(
                "Autonomous operator disabled in configuration; TX-placement \
                 feed (spectral snapshot, decode-history) still runs so the \
                 TX-placement instrument stays live, but no CQ/pounce/collision \
                 TX decisions will be made or dispatched"
            );
        }

        let qso_auto_config = pancetta_qso::AutonomousConfig {
            enabled: config.autonomous.enabled,
            slot_parity: match config.autonomous.slot_parity {
                pancetta_config::autonomous::SlotParitySetting::Even => {
                    pancetta_qso::SlotParityConfig::Even
                }
                pancetta_config::autonomous::SlotParitySetting::Odd => {
                    pancetta_qso::SlotParityConfig::Odd
                }
                pancetta_config::autonomous::SlotParitySetting::Auto => {
                    pancetta_qso::SlotParityConfig::Auto
                }
            },
            cq_after_idle_cycles: config.autonomous.cq_after_idle_cycles,
            cq_no_response_switch_after: config.autonomous.cq_no_response_switch_after,
            max_concurrent_qsos: config.autonomous.max_concurrent_qsos,
            tx_offset_hz: config.autonomous.tx_offset_hz,
            min_dx_score: config.autonomous.min_dx_score,
            min_multi_slot_score: config.autonomous.min_multi_slot_score,
            cq_direction: config.autonomous.cq_direction.clone(),
            listen_cycle: pancetta_qso::autonomous::ListenCycleConfig {
                initial_interval: config.autonomous.listen_cycle.initial_interval,
                backoff_interval: config.autonomous.listen_cycle.backoff_interval,
                collision_interval: config.autonomous.listen_cycle.collision_interval,
                backoff_threshold: config.autonomous.listen_cycle.backoff_threshold,
            },
            band_hopping: pancetta_qso::autonomous::BandHoppingConfig {
                enabled: config.autonomous.band_hopping.enabled,
                hop_threshold: config.autonomous.band_hopping.hop_threshold,
                bands: config
                    .autonomous
                    .band_hopping
                    .bands
                    .iter()
                    .map(|b| pancetta_qso::autonomous::BandEntry {
                        dial_frequency: b.dial_frequency,
                        band_name: b.band_name.clone(),
                        priority: b.priority,
                    })
                    .collect(),
            },
            frequency: pancetta_qso::frequency::FrequencyAllocatorConfig {
                decode_history_cycles: config.autonomous.frequency.decode_history_cycles,
                center_bias_hz: config.autonomous.frequency.center_bias_hz,
                dx_proximity_min_hz: config.autonomous.frequency.dx_proximity_min_hz,
                dx_proximity_max_hz: config.autonomous.frequency.dx_proximity_max_hz,
                min_separation_hz: config.autonomous.frequency.min_separation_hz,
                neighbor_guard_hz: config.autonomous.frequency.neighbor_guard_hz,
                ..Default::default()
            },
            // DX-busy suppression window. Not yet plumbed to pancetta-config;
            // use the AutonomousConfig default (90 s).
            dx_busy_window_secs: pancetta_qso::AutonomousConfig::default().dx_busy_window_secs,
            // DX watchlist (#197) TTL. Not yet plumbed to pancetta-config;
            // use the AutonomousConfig default (150 s / 2.5 min), same
            // precedent as dx_busy_window_secs above.
            watchlist_ttl_secs: pancetta_qso::AutonomousConfig::default().watchlist_ttl_secs,
        };

        let dry_run = config.autonomous.dry_run;
        // FQ-F9: only banner the DRY RUN mode when autonomous is actually
        // engaged — a manual-only operator (autonomous.enabled = false)
        // never reaches `op.decide()`/TX dispatch at all, so a dry-run
        // banner here would misleadingly imply autonomous behavior is
        // active.
        if dry_run && auto_config_enabled {
            warn!(
                target: "autonomous.dry_run",
                "Autonomous DRY RUN mode ENABLED: TransmitRequest / MultiTransmitRequest \
                 from the autonomous operator will be logged but NOT forwarded to the \
                 transmitter. Manual TX (Space-press, --test-tx) is unaffected."
            );
        }

        let our_callsign = config.station.callsign.clone();
        let our_grid = if config.station.grid_square.is_empty() {
            None
        } else {
            Some(config.station.grid_square.clone())
        };

        // Read priority weights before dropping config
        let priority_weights = pancetta_qso::priority::PriorityWeights {
            needed_dxcc: config.autonomous.priorities.needed_dxcc,
            needed_grid: config.autonomous.priorities.needed_grid,
            pota_sota: config.autonomous.priorities.pota_sota,
            rarity: config.autonomous.priorities.rarity,
            signal_strength: config.autonomous.priorities.signal_strength,
            duplicate_penalty: config.autonomous.priorities.duplicate_penalty,
            recent_failure_penalty: config.autonomous.priorities.recent_failure_penalty,
            atno_bonus: config.autonomous.priorities.atno_bonus,
        };

        // FT4-mode dial selection: the config-static band-hop entries carry
        // FT8 dial frequencies. When the active mode is FT4 we override the
        // hop dial with the band's FT4 sub-band frequency at the point of use
        // (the ChangeBand handler below). FT8 mode leaves the configured dial
        // untouched (byte-identical behavior).
        let active_is_ft4 = matches!(
            config.rig.operating_mode(),
            Ok(pancetta_config::rig::OperatingMode::Ft4)
        );

        // Task 16: opt-in auto-repark. Default OFF — read once at startup
        // (mirrors every other config extraction in this fn); a config
        // hot-reload changing this mid-run is out of scope for v1, same as
        // the other autonomous-loop settings extracted here.
        let auto_repark_enabled = config.tx_placement.auto_repark;
        let repark_min_score_gain = config.tx_placement.repark_min_score_gain;
        drop(config);

        let cached_lookup = self.cached_lookup.clone();

        let spot_reporter_callsign = our_callsign.clone();
        let spot_reporter_grid = our_grid.clone();
        let operator = std::sync::Arc::new(tokio::sync::Mutex::new(
            pancetta_qso::AutonomousOperator::new(qso_auto_config, our_callsign, our_grid),
        ));

        // Share the operator's Hold/Auto TX-frequency mode so the smart-freq
        // allocator and collision-listen jitter respect it (Hold → pinned
        // offset, no autonomous moves; Auto → free to choose).
        {
            let op = operator.clone();
            let mut guard = op.lock().await;
            guard.set_tx_freq_mode_source(self.tx_freq_mode.clone());
            // PAN-39: share the generation counter bumped alongside every
            // `tx_freq_mode` store, so `decide_at` can detect a Hold/Auto
            // transition it didn't directly observe.
            guard.set_tx_freq_mode_generation_source(self.tx_freq_mode_generation.clone());
            // FQ-F6: also share the live parked-offset atomic so Hold-mode
            // frequency allocation reflects the operator's actual `o`-modal
            // parked offset, not just the static config value baked in at
            // construction.
            guard.set_tx_offset_hold_source(self.tx_offset_hold_hz());
        }

        // Phase-5 hardening #1: install the same callsign-continuity FP
        // filter the decoder uses, so the TX decision path can reject
        // CQs from callsigns absent from the trust set (defense in
        // depth — the decode-side filter still runs in
        // coordinator/ft8.rs).
        let fp_filter_snapshot = self.fp_filter.read().unwrap().clone();
        if let Some(filter) = fp_filter_snapshot {
            let op = operator.clone();
            let mut guard = op.lock().await;
            guard.set_fp_filter(Some(filter));
            drop(guard);
            info!("Autonomous operator: FP filter installed for TX-side gating");
        } else {
            warn!(
                "Autonomous operator: no FP filter available; CQ responses are NOT \
                 gated by callsign continuity"
            );
        }

        // The channel is created in `new()` (before start_pipeline() spawns
        // the FT8 thread that clones `waterfall_to_auto_tx`); this component
        // just takes the receiver half.
        let waterfall_to_auto_rx = self
            .waterfall_to_auto_rx
            .take()
            .expect("waterfall_to_auto_rx set at coordinator construction");

        let evaluator: std::sync::Arc<dyn pancetta_qso::DxEvaluator> = std::sync::Arc::new(
            pancetta_qso::PriorityScorer::new(priority_weights, Box::new((*cached_lookup).clone())),
        );

        let (_auto_tx, auto_rx) = self
            .message_bus
            .get_or_create_channel(ComponentId::Autonomous)
            .await?;
        let message_bus = self.message_bus.clone();
        let display_feed_enabled = self.display_feed_enabled.clone();

        let cqdx_bridge_for_auto = self.cqdx_bridge.clone();
        // Under `--replay` the cqdx.io *write* path is suppressed (the read
        // path is not) — see `spot_publish_target`, which this is fed into
        // at the one publishing site below.
        let suppress_spot_reports = self.replay_mode();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
        // Split TX dial atomic (0 = simplex). Cleared on autonomous band-hop
        // (same as the manual TUI SetFrequency path).
        let split_tx_hz = self.split_tx_frequency_hz();
        // C9 dedup anchor: record that *pancetta* (the autonomous operator)
        // commanded a band change, so the hamlib poll loop doesn't double-fire
        // the teardown when it reads the new freq back off the rig.
        let last_freq_command = self.last_freq_command.clone();
        let autonomous_runtime_gate = self.autonomous_enabled_runtime.clone();
        // Global tri-state TX policy. Orthogonal to the autonomous runtime
        // gate: autonomous *initiation* (calling CQ ourselves, or
        // hunting/pouncing on a station calling CQ — both carry
        // `qso_id == None` from the decision engine) requires the policy to
        // allow initiation (Full). RespondOnly keeps QSO-in-progress
        // responses (`qso_id == Some`) flowing; Disabled is additionally
        // hard-muted at the TX worker.
        let tx_policy = self.tx_policy.clone();
        // FCC §97.221 presence gate: the operator's last-console-input clock.
        // The autonomous engine may only INITIATE (CQ/pounce) when this is
        // fresh (operator at the console); otherwise respond-only.
        let last_operator_input_ms = self.last_operator_input_ms.clone();
        // Phase 5: the active-QSO set the QSO component maintains. Its length is
        // fed to the operator each slot as `active_qso_count` so the decision
        // engine's `max_concurrent_qsos` gate sees QSOs the engine itself is
        // now driving (autonomous Auto QSOs land here once created), and so we
        // don't open a second pounce while one is already in progress.
        let active_tx_qsos = self.active_tx_qsos.clone();
        // FQ-F3: active QSO id -> TX offset (Hz), maintained by
        // `coordinator/qso.rs`'s event-forwarding task at the same
        // insert/remove points as `active_tx_qsos` above. Synced each tick
        // into the operator's own-frequency registry (see the
        // `set_own_frequencies` call below) so the smart frequency
        // allocator's own-frequency-separation criterion can actually fire —
        // before this wiring, `register_qso_frequency`/
        // `release_qso_frequency` were only ever called from unit tests.
        let active_tx_offsets = self.active_tx_offsets.clone();
        // Task 15: the operator's parked TX-offset atomic (0 = unparked),
        // cloned into the task so the per-slot tick can read it alongside
        // the placement snapshot it already computes (Task 9) and detect a
        // degradation in coverage at the parked bin, independent of the
        // TUI-side transient warning (Task 14).
        let tx_offset_hold_hz = self.tx_offset_hold_hz();
        // PAN-43/45 follow-up (Codex round 2 on PR #342): the
        // queue-independent fallback the QSO task falls back to once its
        // own bounded `AutonomousCqDispatchFailed` message-bus retries are
        // exhausted -- drained once per `slot_interval` tick below,
        // independent of this task's own message-bus channel entirely.
        let pending_autonomous_cq_dispatch_failures =
            self.pending_autonomous_cq_dispatch_failures.clone();
        // PAN-72: the other half of `pending_qso_offset_requests` (see its
        // own doc comment on the field) -- pushed by the QSO event-forwarder
        // task, drained here once per `slot_interval` tick, same
        // push-mailbox shape as `pending_autonomous_cq_dispatch_failures`
        // above.
        let pending_qso_offset_requests = self.pending_qso_offset_requests.clone();
        // PAN-72 (Codex round 1 on PR #350, finding 6): the shared Hold/Auto
        // atomic, re-read INSIDE the drain at commit time. The operator can
        // press `f` for Hold after an action was queued but before this
        // once-per-slot tick drains it; without the recheck the commit would
        // move a QSO Hold had just pinned.
        let tx_freq_mode_for_drain = self.tx_freq_mode.clone();
        // PAN-72: the `u` "nudge" keystroke's CQ-hunting fallback (no QSO
        // active at dispatch time) -- a one-shot flag the tui_relay task
        // sets; drained here once per `slot_interval` tick, alongside
        // `pending_qso_offset_requests` above, and forwarded into
        // `AutonomousOperator::request_manual_switch` (consumed
        // unconditionally at the top of `decide_at` itself, so this doesn't
        // need any restart-safety handling of its own -- `op` lives inside
        // THIS task and is fully re-created on an Autonomous-task restart,
        // unlike the cross-component `QsoManager` handle above).
        let pending_cq_offset_nudge = self.pending_cq_offset_nudge.clone();
        // PAN-72 (fixed after task-review Critical finding): the Autonomous
        // task doesn't otherwise hold any `QsoManager` handle. A plain
        // `self.qso_manager_for_supervisor.clone()` here would capture
        // whatever manager exists at THIS spawn time only -- health.rs's
        // supervisor can independently restart the Qso component without
        // restarting Autonomous, which reassigns
        // `qso_manager_for_supervisor` to a brand-new `QsoManager` with an
        // empty QSO map (see `start_qso_component`, qso.rs). A stale
        // captured clone would then have every subsequent lookup miss
        // ("QSO likely completed") for the rest of the process's uptime.
        // Instead, subscribe to the `qso_manager_watch` channel
        // (`mod.rs`), which `start_qso_component` sends the fresh handle
        // into on every (re)start -- `.subscribe()` seeds this `Receiver`
        // with whatever's most recently been sent, so this is correct
        // whether Qso started before or after this component. The tick
        // loop below re-reads it fresh via `.borrow().clone()` every
        // cycle, not just once here. `apply_tx_offset_switch` doesn't read
        // `tx_freq_mode`, so Task 8's disconnected-tx_freq_mode-Arc caveat
        // is unrelated to this handle.
        let qso_manager_watch = self.qso_manager_watch.subscribe();
        let auto_handle = {
            let shutdown = self.shutdown_signal.clone();
            let operator = operator.clone();
            let evaluator = evaluator.clone();

            tokio::spawn(async move {
                info!("Autonomous operator started");
                if suppress_spot_reports && cqdx_bridge_for_auto.is_some() {
                    info!("Replay mode: cqdx.io spot reporting suppressed -- replayed decodes are not live spots");
                }

                let mut slot_messages: Vec<pancetta_qso::DecodedMessageInfo> = Vec::new();
                // PAN-38 round 1: qso_id -> cq_attempt_id for self-CQs whose
                // `start_cq` succeeded but haven't yet been confirmed
                // transmitted (see `QsoMessage::AutonomousCqOpened`). Entries
                // are removed on the matching `TransmitComplete` (success or
                // failure) so this never grows past the number of self-CQs
                // genuinely in flight at once (in practice at most one or
                // two, since a self-CQ only fires when idle).
                let mut pending_self_cq_qsos: std::collections::HashMap<String, u64> =
                    std::collections::HashMap::new();
                // PAN-43/45 follow-up (Codex round 3 on PR #342): the
                // queue-independent fallback drain (below, once per
                // `slot_interval` tick) can run BEFORE a same-attempt
                // `AutonomousCqOpened` that's still sitting queued on the
                // message bus gets processed -- the fallback bypasses the
                // bus entirely, so it no longer inherits the send-order
                // guarantee a single channel gives the two normal,
                // sequentially-sent messages (`AutonomousCqOpened` then
                // `AutonomousCqDispatchFailed`) relative to each other.
                // Without this, that late-processed Open would insert a
                // `pending_self_cq_qsos` entry for a QSO that was already
                // rolled back and never actually created -- no
                // `TransmitComplete` will ever arrive to remove it, so
                // repeated races would grow the map unboundedly. Every
                // rollback (fallback or message-driven) records its
                // attempt_id here; the Open handler checks it before
                // inserting. Bounded FIFO, not a `HashSet` -- self-CQ
                // attempts run at most one or two at a time in practice, so
                // this is a defensive cap against pathological growth (a
                // registration that's declined outright, PAN-45, never has
                // a matching Open to consume its tombstone at all), not a
                // realistic operating ceiling.
                let mut rolled_back_attempt_tombstones: std::collections::VecDeque<u64> =
                    std::collections::VecDeque::new();
                // Task 15: coordinator-local edge-trigger baseline for the
                // persistent tx.placement DiagnosticEvent, scoped to THIS
                // task's own loop. Deliberately separate from the TUI-side
                // `App::park_coverage_last` (Task 14) — different process,
                // different update cadence, different purpose (retained
                // diagnostic history vs. a transient status line) — the two
                // are never unified.
                let mut last_coverage: Option<u8> = None;
                let mut gate_diag_state = GateDiagState::default();
                let mut skip_seen = SkipDiagSeen::default();
                // Align slot timer to FT8 UTC boundaries (0/15/30/45 seconds)
                // with sub-second precision. tokio::time::interval_at then
                // keeps the cadence exact every 15s relative to that first tick.
                let now_utc = chrono::Utc::now();
                let next_slot =
                    pancetta_core::slot::next_slot_start(now_utc, chrono::Duration::zero());
                let initial_delay = pancetta_core::slot::duration_until(next_slot, now_utc);
                let mut slot_interval = tokio::time::interval_at(
                    tokio::time::Instant::now() + initial_delay,
                    Duration::from_secs(15),
                );
                slot_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

                loop {
                    tokio::select! {
                        _ = slot_interval.tick() => {
                            // Report decoded spots to cqdx.io (never under
                            // `--replay` -- see `suppress_spot_reports`).
                            if let Some(bridge) =
                                spot_publish_target(cqdx_bridge_for_auto.as_ref(), suppress_spot_reports)
                            {
                                let dial_freq = operating_frequency_hz.load(Ordering::Relaxed);
                                let spot_reports: Vec<pancetta_cqdx::SpotReport> = slot_messages
                                    .iter()
                                    .filter_map(|msg| {
                                        msg.callsign.as_ref().map(|call| pancetta_cqdx::SpotReport {
                                            callsign: call.clone(),
                                            grid: None,
                                            frequency: dial_freq + msg.frequency_hz as u64,
                                            mode: "FT8".to_string(),
                                            snr: msg.snr,
                                            timestamp: chrono::Utc::now(),
                                            reporter: spot_reporter_callsign.clone(),
                                            reporter_grid: spot_reporter_grid.clone(),
                                        })
                                    })
                                    .collect();
                                bridge.report_spots(spot_reports);
                            }

                            let mut op = operator.lock().await;

                            // PAN-43/45 follow-up (Codex round 2 on PR
                            // #342): drain any self-CQ dispatch failures
                            // the QSO task couldn't deliver through the
                            // message bus even after its own bounded
                            // retries (sustained backpressure on this
                            // task's own channel) -- queue-independent, so
                            // this always succeeds regardless of how
                            // congested that channel currently is. Mirrors
                            // exactly what the message-driven
                            // `AutonomousCqDispatchFailed` handler below
                            // does.
                            drain_pending_autonomous_cq_dispatch_failures(
                                &mut op,
                                &mut pending_self_cq_qsos,
                                &pending_autonomous_cq_dispatch_failures,
                                &mut rolled_back_attempt_tombstones,
                            );

                            // PAN-72: resolve and commit any mid-QSO
                            // TX-offset actions the QSO event-forwarder
                            // queued this cycle (stall-switch/revert).
                            // Re-borrow the watch channel FRESH every tick
                            // (not a captured clone) so a Qso-only
                            // component restart is picked up immediately --
                            // see the doc comment on `qso_manager_watch`'s
                            // subscribe site above. A `None` value here
                            // means the Qso component isn't up yet, which
                            // also means nothing could have been queued --
                            // true no-op, not a bug. Clone out of the
                            // `Ref` and drop it (a plain owned-value `let`,
                            // not `borrow().clone()` inline in the `if
                            // let`'s scrutinee) BEFORE the `.await` below --
                            // otherwise the watch channel's internal read
                            // lock would be held across the drain's own
                            // `.await` points, needlessly blocking a
                            // concurrent `start_qso_component`'s `.send()`.
                            let current_qso_manager = qso_manager_watch.borrow().clone();
                            if let Some(qso_manager) = current_qso_manager {
                                drain_pending_qso_offset_requests(
                                    &mut op,
                                    &qso_manager,
                                    &pending_qso_offset_requests,
                                    &active_tx_offsets,
                                    &tx_freq_mode_for_drain,
                                )
                                .await;
                            }

                            // PAN-72: the `u` nudge's CQ-hunting fallback --
                            // forwarded into `request_manual_switch`, which
                            // is itself only consumed inside `decide_at`
                            // (called below via `op.decide()` when
                            // `auto_config_enabled`). A manual-only operator
                            // (`auto_config_enabled == false`) simply leaves
                            // this pending on `op` until decide() actually
                            // runs again -- not lost, just deferred, same as
                            // any other field `decide_at` alone consumes.
                            if pending_cq_offset_nudge.swap(false, Ordering::Relaxed) {
                                op.request_manual_switch();
                            }

                            // Update spectral data from waterfall
                            if let Ok(rows) = waterfall_to_auto_rx.try_recv() {
                                if let Some(first_row) = rows.first() {
                                    let num_bins = first_row.len();
                                    let mut avg = vec![0.0f32; num_bins];
                                    for row in &rows {
                                        for (i, &v) in row.iter().enumerate().take(num_bins) {
                                            avg[i] += v;
                                        }
                                    }
                                    let n = rows.len() as f32;
                                    for v in &mut avg {
                                        *v /= n;
                                    }
                                    op.update_spectral(pancetta_qso::frequency::SpectralSnapshot {
                                        power_bins: avg,
                                        // The waterfall's bins start at 0 Hz
                                        // (see `pancetta-ft8/src/decoder.rs`'s
                                        // `bin_start = 0usize`); this label
                                        // must match that real axis so
                                        // `power_near`/`peak_near`'s bin-index
                                        // math reads the correct bin (FQ-F1).
                                        freq_min_hz: 0.0,
                                        freq_max_hz: 3000.0,
                                    });
                                }
                            }

                            if let Some(ref bridge) = cqdx_bridge_for_auto {
                                let spot_freqs = bridge.spot_frequencies().await;
                                op.update_live_spots(&spot_freqs);
                            }

                            op.feed_decoded_messages(&slot_messages, evaluator.as_ref());

                            // DX watchlist (#197): housekeeping broadcast every
                            // tick, same cadence as the TX-placement instrument
                            // below — sent regardless of whether the list is
                            // empty, so the TUI side can bulk-resync (self-
                            // healing; never diffed).
                            {
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Tui,
                                    MessageType::DxWatchlistUpdate {
                                        callsigns: op.watchlist_callsigns(),
                                    },
                                    Instant::now(),
                                );
                                let _ = message_bus.send_message(msg).await;
                            }

                            // Task 16: opt-in auto-repark inputs, captured
                            // (if this tick has a snapshot) BEFORE
                            // `snapshot` moves into the TxPlacementUpdate
                            // message below — the decision itself is made
                            // further down, after the freshest possible
                            // read of `active_tx_qsos` (see there for why).
                            // `None` when this tick has no snapshot yet
                            // (`should_repark` correctly holds on `None`).
                            let mut repark_parked_hz: Option<u64> = None;
                            let mut repark_coverage: Option<u8> = None;
                            let mut repark_parked_score: Option<f64> = None;
                            let mut repark_best: Option<
                                pancetta_qso::frequency::FrequencyCandidate,
                            > = None;

                            // TX-placement instrument feed (docs/superpowers/specs/
                            // 2026-07-03-tui-redesign-design.md §2): per-window
                            // read of the SAME allocator/history the autonomous
                            // path just used above — not a separate computation
                            // (single-scorer invariant). Sent regardless of
                            // whether autonomous mode is enabled (housekeeping).
                            if let Some(snapshot) = op.placement_snapshot(10) {
                                // Task 15: persistent counterpart to the TUI's
                                // transient parked-slice degradation warning
                                // (Task 14). The TUI status line is missed if
                                // the operator isn't looking at the moment it
                                // fires; this lands the same finding in the
                                // retained Shift+D diagnostic history. Reads
                                // the SAME snapshot the TxPlacementUpdate below
                                // carries (single-scorer invariant) and the
                                // coordinator's own copy of the operator's
                                // parked offset — a coordinator-local
                                // edge-trigger (`last_coverage`), independent
                                // of the TUI-side `park_coverage_last`.
                                let parked_hz = tx_offset_hold_hz.load(Ordering::Relaxed);
                                let coverage = parked_bin_coverage(parked_hz, &snapshot);
                                if let (Some(prev), Some(code)) = (last_coverage, coverage) {
                                    if code < prev {
                                        let text = format!(
                                            "Parked TX offset {parked_hz} Hz coverage \
                                             degraded (openness {prev} -> {code})"
                                        );
                                        let diag_msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Tui,
                                            MessageType::DiagnosticEvent {
                                                target: "tx.placement",
                                                level: pancetta_core::DiagnosticLevel::Warn,
                                                text,
                                                qso_id: None,
                                                callsign: None,
                                            },
                                            Instant::now(),
                                        );
                                        let _ = message_bus.send_message(diag_msg).await;
                                    }
                                }
                                last_coverage = coverage;

                                // Task 16: capture the repark inputs against
                                // THIS snapshot before it moves into the
                                // TxPlacementUpdate message just below.
                                repark_parked_hz = Some(parked_hz);
                                repark_coverage = coverage;
                                repark_parked_score =
                                    parked_score_in_slices(parked_hz, &snapshot);
                                repark_best = snapshot.slices.first().cloned();

                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Tui,
                                    MessageType::TxPlacementUpdate { snapshot },
                                    Instant::now(),
                                );
                                let _ = message_bus.send_message(msg).await;
                            }

                            // Phase 5: sync the operator's active-QSO count from
                            // the shared active-QSO set so `max_concurrent_qsos`
                            // gating is honored (fail-open to 0 on a poisoned
                            // lock — the engine's own dedup/in-progress gates
                            // still apply).
                            let active_now = active_tx_qsos
                                .read()
                                .map(|s| s.len() as u32)
                                .unwrap_or(0);
                            op.set_active_qso_count(active_now);

                            // FQ-F3: sync the own-frequency registry from the
                            // coordinator-level snapshot each tick. Bulk
                            // replace (not diff-based register/release) —
                            // see `FrequencyAllocator::set_own_frequencies`'s
                            // doc comment for why: it can never leak a
                            // stale entry, since every tick's wholesale
                            // replace self-heals any drift. Fail-open to an
                            // empty map on a poisoned lock, matching
                            // `active_now`'s own fail-open convention just
                            // above (this is the same non-safety-critical
                            // scoring input, not the TX-safety gate below).
                            let own_freqs = active_tx_offsets
                                .read()
                                .map(|m| m.clone())
                                .unwrap_or_default();
                            op.frequency_allocator_mut()
                                .set_own_frequencies(own_freqs);

                            // Task 16: opt-in auto-repark (default OFF —
                            // `auto_repark_enabled` is inert unless the
                            // operator sets `[tx_placement].auto_repark =
                            // true`). LIVE-STREAM SAFETY: read
                            // `active_tx_qsos` again here, freshly, with NO
                            // `.await` between this read and the
                            // `tx_offset_hold_hz.store` below — every
                            // statement in between is synchronous Rust, so
                            // this is the freshest possible read of the
                            // shared set relative to the write; the gate
                            // cannot fire against a state that went live
                            // between the decision and the write.
                            //
                            // Whole-branch-review fix (finding 2): fail
                            // CLOSED on a poisoned lock. Deliberately a
                            // SEPARATE read from `active_now` above —
                            // `active_now` only feeds the operator's
                            // `max_concurrent_qsos` count (a different,
                            // non-safety concern, documented there as
                            // intentionally fail-OPEN to 0). This one is
                            // the plan's explicitly-labeled "LIVE STREAM
                            // SAFETY" gate, so a lock-read error is treated
                            // as "assume a QSO may be active" — skip the
                            // repark this tick — matching the codebase's
                            // established fail-closed convention for
                            // TX-adjacent gates under lock/state
                            // uncertainty (the remote-TX arm gate,
                            // `coordinator/tx.rs`). `should_repark` ALSO
                            // re-checks `active_qsos > 0` internally — this
                            // is belt-and-suspenders, not a substitute for
                            // the freshness/fail-closed-ness of this read.
                            let repark_active_qsos =
                                resolve_repark_active_qsos(active_tx_qsos.read().map(|s| s.len()));
                            if let Some(new_hz) = should_repark(
                                auto_repark_enabled,
                                repark_active_qsos,
                                repark_parked_hz.unwrap_or(0),
                                repark_coverage,
                                repark_parked_score,
                                repark_best.as_ref(),
                                repark_min_score_gain,
                            ) {
                                let old_hz = repark_parked_hz.unwrap_or(0);
                                tx_offset_hold_hz.store(new_hz, Ordering::Relaxed);
                                info!(
                                    target: "tx.placement",
                                    "auto-reparked {old_hz} Hz -> {new_hz} Hz"
                                );
                                let text =
                                    format!("Auto-reparked TX offset {old_hz} Hz -> {new_hz} Hz");
                                let status_msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Tui,
                                    MessageType::StatusUpdate(text.clone()),
                                    Instant::now(),
                                );
                                let _ = message_bus.send_message(status_msg).await;
                                let diag_msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Tui,
                                    MessageType::DiagnosticEvent {
                                        target: "tx.placement",
                                        level: pancetta_core::DiagnosticLevel::Info,
                                        text,
                                        qso_id: None,
                                        callsign: None,
                                    },
                                    Instant::now(),
                                );
                                let _ = message_bus.send_message(diag_msg).await;
                                // Whole-branch-review fix (finding 1): echo
                                // the new offset back to the TUI. Every
                                // OTHER writer of `tx_offset_hold_hz` is
                                // TUI-initiated and updates `App`'s own
                                // copy directly; auto-repark is
                                // coordinator-initiated, so without this
                                // the TUI's park line / HOLD chip / Task
                                // 14 degradation baseline would keep
                                // referencing the OLD frequency forever.
                                let offset_msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Tui,
                                    MessageType::TxOffsetStatus { offset_hz: new_hz },
                                    Instant::now(),
                                );
                                let _ = message_bus.send_message(offset_msg).await;
                            }

                            let listen_messages = slot_messages.clone();
                            slot_messages.clear();

                            // FQ-F9: decision-making (`op.decide()`) and
                            // everything downstream of it (the whole
                            // OperatorAction/Transmit dispatch block below,
                            // including single-TX and multi-TX bundling) is
                            // gated on `auto_config_enabled`. A manual-only
                            // operator (autonomous.enabled = false) must see
                            // zero Transmit/OperatorAction/MessageToSend-
                            // shaped output escape to the message bus — the
                            // housekeeping above (spectral snapshot,
                            // feed_decoded_messages, TxPlacementUpdate,
                            // active-QSO/own-frequency sync, auto-repark)
                            // already ran unconditionally so the
                            // TX-placement instrument stays live regardless.
                            if !auto_config_enabled {
                                // Nothing further needed from the operator
                                // lock this tick; drop it promptly rather
                                // than holding it through `decide()`/dispatch.
                                drop(op);
                            } else {
                            let actions = op.decide();
                            let skip_records = op.take_skip_log();
                            // PAN-38: captured before `drop(op)` — if a
                            // self-CQ reached the radio this cycle (not
                            // suppressed below), tag its `StartAutonomousQso`
                            // dispatch with this id so a downstream dispatch
                            // failure can be rolled back to exactly this
                            // attempt's pre-mutation state.
                            let self_cq_attempt_id = op.last_cq_attempt_id();
                            drop(op);

                            for record in skip_records {
                                if !should_report_skip(
                                    &mut skip_seen,
                                    skip_reason_key(&record.reason),
                                    record.callsign.as_deref(),
                                    std::time::Instant::now(),
                                ) {
                                    continue;
                                }
                                let diagnostic = skip_record_diagnostic(&record);
                                crate::coordinator::tx::emit_diagnostic_full(
                                    &message_bus,
                                    ComponentId::Autonomous,
                                    diagnostic.target,
                                    diagnostic.level,
                                    diagnostic.text,
                                    None,
                                    record.callsign.as_deref(),
                                )
                                .await;
                            }

                            // Collect Transmit actions, then bundle into a
                            // single MultiTransmitRequest (or single TransmitRequest).
                            let mut tx_items: Vec<(crate::message_bus::TransmitRequestItem, Option<pancetta_core::slot::SlotParity>)> = Vec::new();
                            // Codex review (PR #276): captured while actions are
                            // still available (tx_items is moved into
                            // plan_slot_transmissions below) so we can tell,
                            // after gating, whether THIS cycle's self-CQ
                            // (qso_id: None, CQ-shaped text — distinct from a
                            // pounce opening) actually reached the radio.
                            let mut self_cq_emitted = false;

                            for action in actions {
                                match action {
                                    pancetta_qso::OperatorAction::Transmit {
                                        ref message_text,
                                        frequency_offset,
                                        ref qso_id,
                                        tx_parity,
                                    } => {
                                        if qso_id.is_none() {
                                            info!(
                                                "Autonomous: opening slot at {:.0} Hz: {}",
                                                frequency_offset, message_text
                                            );
                                            // Codex review (PR #276, round 6):
                                            // match the CQ token exactly, not
                                            // just a byte prefix — a pounce
                                            // reply to a valid callsign like
                                            // "CQ7ABC" ("CQ7ABC <us> <grid>")
                                            // would otherwise satisfy
                                            // starts_with("CQ") and be
                                            // misidentified as a self-CQ,
                                            // corrupting a later
                                            // restore_cq_state() call with
                                            // the wrong action's snapshot.
                                            if message_text.split_whitespace().next() == Some("CQ")
                                            {
                                                self_cq_emitted = true;
                                            }
                                        }
                                        tx_items.push((
                                            crate::message_bus::TransmitRequestItem {
                                                message_text: message_text.clone(),
                                                frequency_offset,
                                                qso_id: qso_id.clone(),
                                            },
                                            tx_parity,
                                        ));
                                    }
                                    pancetta_qso::OperatorAction::ChangeBand { dial_frequency } => {
                                        // FT4-mode dial override: the band-hop
                                        // config carries FT8 dials. In FT4 mode,
                                        // resolve the hopped band and substitute
                                        // its FT4 sub-band frequency. If the band
                                        // has no FT4 frequency (e.g. 60m/160m) or
                                        // can't be resolved, keep the configured
                                        // (FT8) dial and warn. FT8 mode never
                                        // enters this branch (byte-identical).
                                        let dial_frequency = if active_is_ft4 {
                                            match pancetta_core::Band::from_frequency(dial_frequency)
                                                .and_then(|b| b.ft4_frequency())
                                            {
                                                Some(ft4) => ft4,
                                                None => {
                                                    warn!(
                                                        target: "operator.override",
                                                        "FT4 mode: no FT4 dial for band-hop target {} Hz; using configured (FT8) dial",
                                                        dial_frequency
                                                    );
                                                    dial_frequency
                                                }
                                            }
                                        } else {
                                            dial_frequency
                                        };
                                        // C9 — the autonomous operator is changing
                                        // band. An active QSO can't complete on the
                                        // new band, so tear active QSOs down (same
                                        // mechanism as the TUI SetFrequency path)
                                        // before/at the band switch. Capture the
                                        // *old* dial freq, update the shared atomic,
                                        // and stamp the dedup anchor so the hamlib
                                        // poll loop doesn't double-fire when it reads
                                        // the new freq back off the rig.
                                        let old_freq_hz =
                                            operating_frequency_hz.load(Ordering::Relaxed);
                                        operating_frequency_hz
                                            .store(dial_frequency, Ordering::Relaxed);
                                        if let Ok(mut anchor) = last_freq_command.lock() {
                                            *anchor = Some((dial_frequency, Instant::now()));
                                        }
                                        if crate::coordinator::is_band_change(
                                            old_freq_hz,
                                            dial_frequency,
                                        ) {
                                            info!(
                                                target: "operator.override",
                                                "Autonomous band change {} Hz -> {} Hz — tearing down active QSOs",
                                                old_freq_hz, dial_frequency
                                            );
                                            let teardown = ComponentMessage::new(
                                                ComponentId::Autonomous,
                                                ComponentId::Qso,
                                                MessageType::QsoMessage(
                                                    crate::message_bus::QsoMessage::BandChanged {
                                                        previous_hz: old_freq_hz,
                                                        new_hz: dial_frequency,
                                                    },
                                                ),
                                                Instant::now(),
                                            );
                                            if let Err(e) = message_bus.send_message(teardown).await {
                                                warn!(
                                                    "Autonomous band change: failed to send teardown: {}",
                                                    e
                                                );
                                            }
                                            // A band change invalidates any split TX freq.
                                            if split_tx_hz.swap(0, Ordering::Relaxed) != 0 {
                                                // Push authoritative split clear to the TUI chip
                                                // via the message bus (Autonomous → Tui relay).
                                                let split_clr_tui = ComponentMessage::new(
                                                    ComponentId::Autonomous,
                                                    ComponentId::Tui,
                                                    MessageType::SplitStatus { tx_hz: 0 },
                                                    Instant::now(),
                                                );
                                                let _ = message_bus.send_message(split_clr_tui).await;
                                                super::remote_gateway::relay_to_gateway(
                                                    &message_bus,
                                                    &display_feed_enabled,
                                                    ComponentId::Autonomous,
                                                    MessageType::SplitStatus { tx_hz: 0 },
                                                )
                                                .await;
                                                let clr = ComponentMessage::new(
                                                    ComponentId::Autonomous,
                                                    ComponentId::Hamlib,
                                                    MessageType::RigControl(
                                                        crate::message_bus::RigControlMessage::SetSplit {
                                                            enabled: false,
                                                            tx_frequency: 0,
                                                        },
                                                    ),
                                                    Instant::now(),
                                                );
                                                let _ = message_bus.send_message(clr).await;
                                            }
                                        }
                                        let msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Hamlib,
                                            MessageType::RigControl(
                                                crate::message_bus::RigControlMessage::SetFrequency {
                                                    vfo: 0,
                                                    frequency: dial_frequency,
                                                },
                                            ),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(msg).await {
                                            warn!("Failed to send ChangeBand: {}", e);
                                        }
                                    }
                                    pancetta_qso::OperatorAction::StatusUpdate(status) => {
                                        let msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Tui,
                                            MessageType::AutonomousStatus(
                                                crate::message_bus::AutonomousStatusData {
                                                    enabled: status.enabled,
                                                    state: status.state,
                                                    slot_parity: status.slot_parity,
                                                    listen_counter: status.listen_counter,
                                                    active_qsos: status.active_qsos,
                                                    max_qsos: status.max_qsos,
                                                    idle_cycles: status.idle_cycles,
                                                    band_name: status.band_name,
                                                    tx_offset_hz: status.tx_offset_hz,
                                                },
                                            ),
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(msg).await {
                                            warn!("Failed to send AutonomousStatus: {}", e);
                                        }
                                    }
                                    pancetta_qso::OperatorAction::Listen => {}
                                    pancetta_qso::OperatorAction::CollisionListen => {
                                        // Process collision listen with decoded messages from this slot
                                        // to detect interference on our TX frequency.
                                        let mut op = operator.lock().await;
                                        let collision_actions =
                                            op.process_collision_listen(&listen_messages);
                                        drop(op);
                                        // Re-inject any resulting actions (e.g., FrequencyShift)
                                        for ca in collision_actions {
                                            if let pancetta_qso::OperatorAction::FrequencyShift { new_offset_hz } = ca {
                                                info!("Collision listen: TX offset shifted to {:.0} Hz", new_offset_hz);
                                            }
                                        }
                                    }
                                    pancetta_qso::OperatorAction::FrequencyShift { new_offset_hz } => {
                                        info!("Autonomous: TX offset shifted to {:.0} Hz", new_offset_hz);
                                    }
                                }
                            }

                            // Gate + route this slot's TX items (Shift+Q runtime
                            // gate → tri-state TX policy → opening→QSO-start
                            // split). All the decision logic is the pure
                            // `plan_slot_transmissions`; here we only do I/O.
                            let runtime_gate_open =
                                autonomous_runtime_gate.load(Ordering::Acquire);
                            let policy = pancetta_core::TxPolicy::from_u8(
                                tx_policy.load(Ordering::Acquire),
                            );
                            // FCC §97.221 presence gate: autonomous *initiation*
                            // (CQ-self + pounce) is only allowed when the
                            // operator has interacted with the console within
                            // OPERATOR_PRESENCE_WINDOW. Headless / idle → not
                            // present → respond-only initiation (the station may
                            // still continue in-progress QSOs).
                            let operator_present = super::operator_present_now(
                                &last_operator_input_ms,
                            );
                            let plan = plan_slot_transmissions(
                                tx_items,
                                runtime_gate_open,
                                policy,
                                dry_run,
                                &listen_messages,
                                operator_present,
                                self_cq_attempt_id,
                            );

                            // hb-161: the operator pressed Shift+Q — log the
                            // disengagement once so it is visible in journals.
                            if plan.runtime_gate_dropped > 0 {
                                warn!(
                                    target: "operator.override",
                                    "Autonomous runtime gate is OFF; dropping {} TX item(s) \
                                     produced this cycle (operator pressed Shift+Q)",
                                    plan.runtime_gate_dropped
                                );
                            }
                            if plan.policy_dropped > 0 {
                                info!(
                                    target: "tx.policy",
                                    "TX policy {}: suppressing {} autonomous initiation \
                                     item(s) this cycle (QSO-in-progress items kept)",
                                    policy.label(),
                                    plan.policy_dropped
                                );
                            }
                            if plan.presence_dropped > 0 {
                                info!(
                                    target: "tx.policy",
                                    "Operator-presence gate (FCC §97.221): no console \
                                     activity in the last {}s — suppressing {} autonomous \
                                     initiation item(s); responding/in-progress kept. \
                                     Press any key at the station to resume initiation.",
                                    super::OPERATOR_PRESENCE_WINDOW.as_secs(),
                                    plan.presence_dropped
                                );
                            }

                            // Codex review (PR #276): `decide_at` counted this
                            // cycle's self-CQ toward the no-response streak
                            // optimistically, before any of these gates ran.
                            // Mirrors `plan_slot_transmissions`'s own suppression
                            // conditions exactly (runtime gate drops everything;
                            // policy+presence drop qso_id:None items; dry_run
                            // diverts openings to `dry_run_openings` instead of
                            // `qso_starts`) rather than re-deriving from `plan`,
                            // so it can never disagree with what actually got
                            // gated. If none of those apply, the CQ genuinely
                            // reached the radio and the streak stands.
                            if self_cq_suppressed(
                                self_cq_emitted,
                                runtime_gate_open,
                                policy,
                                operator_present,
                                dry_run,
                            ) {
                                let mut op = operator.lock().await;
                                op.restore_cq_state();
                                drop(op);
                            }
                            for diagnostic in
                                gate_diagnostics_for_slot(
                                    &mut gate_diag_state,
                                    &plan,
                                    runtime_gate_open,
                                    policy,
                                    operator_present,
                                )
                            {
                                crate::coordinator::tx::emit_diagnostic_full(
                                    &message_bus,
                                    ComponentId::Autonomous,
                                    diagnostic.target,
                                    diagnostic.level,
                                    diagnostic.text,
                                    None,
                                    None,
                                )
                                .await;
                            }
                            for text in &plan.dry_run_openings {
                                info!(
                                    target: "autonomous.dry_run",
                                    "DRY RUN: would have opened autonomous QSO from '{}'",
                                    text
                                );
                            }

                            // Phase 5: open each surviving autonomous QSO via the
                            // QSO component (the QsoManager owns the exchange and
                            // emits the opening TX + StateChanged). Sent INSTEAD
                            // OF a raw TransmitRequest — no double-send.
                            for start in plan.qso_starts {
                                let cq_attempt_id = start.cq_attempt_id;
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Qso,
                                    MessageType::QsoMessage(
                                        crate::message_bus::QsoMessage::StartAutonomousQso {
                                            callsign: start.callsign,
                                            frequency: start.frequency,
                                            parity: start.parity,
                                            cq_attempt_id: start.cq_attempt_id,
                                        },
                                    ),
                                    Instant::now(),
                                );
                                // PAN-38 round 5 (Codex): `send_message` swallows a
                                // dropped (bounded-queue-full) delivery into
                                // `Ok(())`, indistinguishable from success -- the
                                // `if let Err` check below can therefore never
                                // detect a drop. `send_message_checked` surfaces
                                // it, so a genuine self-CQ (cq_attempt_id: Some)
                                // that never reached the QSO task rolls back its
                                // speculative streak/offset mutation exactly like
                                // a downstream dispatch failure does, instead of
                                // leaving it permanently uncorrected.
                                match message_bus.send_message_checked(msg).await {
                                    Ok(true) => {}
                                    Ok(false) => {
                                        warn!(
                                            "StartAutonomousQso dropped (channel full or \
                                             disconnected)"
                                        );
                                        if let Some(attempt_id) = cq_attempt_id {
                                            let mut op = operator.lock().await;
                                            op.restore_cq_state_for_attempt(attempt_id);
                                            drop(op);
                                            pending_self_cq_qsos
                                                .retain(|_, id| *id != attempt_id);
                                        }
                                    }
                                    Err(e) => {
                                        warn!("Failed to send StartAutonomousQso: {}", e);
                                    }
                                }
                            }

                            let mut tx_items = plan.tx_items;

                            // Bundle collected TX items into a single message.
                            if tx_items.len() == 1 {
                                let (item, tx_parity) = tx_items.remove(0);
                                if dry_run {
                                    info!(
                                        target: "autonomous.dry_run",
                                        "DRY RUN: would have transmitted '{}' at offset {:.0} Hz (qso_id={:?}, parity={:?})",
                                        item.message_text,
                                        item.frequency_offset,
                                        item.qso_id,
                                        tx_parity
                                    );
                                } else {
                                    let msg = ComponentMessage::new(
                                        ComponentId::Autonomous,
                                        ComponentId::Ft8Transmitter,
                                        MessageType::TransmitRequest {
                                            message_text: item.message_text,
                                            frequency_offset: item.frequency_offset,
                                            qso_id: item.qso_id,
                                            tx_parity,
                                            origin: crate::message_bus::TxOrigin::Local,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        warn!("Failed to send TransmitRequest: {}", e);
                                    }
                                }
                            } else if tx_items.len() > 1 {
                                // Anchor on the first-seen item's parity (mirrors the
                                // TX-worker coalescer's "oldest-retained stream wins"
                                // rule — see `coalesce_transmit_requests` in tx.rs).
                                // A later item whose CONCRETE parity disagrees with the
                                // anchor must NOT be silently folded into this bundle
                                // (that would put it in the wrong slot window — the
                                // exact class of bug this invariant exists to prevent).
                                // Exclude it from the bundle instead; it still goes out,
                                // just via the normal single-item TX path below, on its
                                // own parity.
                                let bundle_parity = tx_items[0].1;
                                let mut bundled = Vec::with_capacity(tx_items.len());
                                let mut excluded = Vec::new();
                                for (idx, (item, p)) in tx_items.into_iter().enumerate() {
                                    let disagrees = match (bundle_parity, p) {
                                        (Some(anchor), Some(this)) => anchor != this,
                                        _ => false,
                                    };
                                    if idx > 0 && disagrees {
                                        warn!(
                                            target: "pancetta::tx.policy",
                                            "Multi-TX item {} (qso_id={:?}) has tx_parity {:?}, \
                                             bundle is anchored to {:?} (first-seen item); \
                                             excluding it from this bundle instead of \
                                             coercing it onto the wrong slot — it will be sent \
                                             individually on its own parity",
                                            idx, item.qso_id, p, bundle_parity
                                        );
                                        excluded.push((item, p));
                                    } else {
                                        bundled.push((item, p));
                                    }
                                }

                                // FQ-F4/TX-F6 defense-in-depth: pairwise
                                // frequency-separation check, mirroring the
                                // TX-worker coalescer's belt-and-suspenders
                                // check (see `coalesce_transmit_requests` in
                                // tx.rs). `modulate_multi_tx` fails the WHOLE
                                // bundle — not just the colliding pair — if
                                // any two folded streams' offsets are closer
                                // than its minimum separation (bandwidth + 25
                                // Hz guard). Fix 5's own de-confliction at
                                // QSO-open time should make this rare; this
                                // is cheap insurance. Exclude (don't coerce)
                                // a later item too close to an
                                // already-bundled one — it still transmits,
                                // individually, via the same single-TX path
                                // below, alongside any parity-excluded items.
                                let mut freq_checked: Vec<(
                                    crate::message_bus::TransmitRequestItem,
                                    Option<pancetta_core::slot::SlotParity>,
                                )> = Vec::with_capacity(bundled.len());
                                for (item, p) in bundled.into_iter() {
                                    let too_close = freq_checked.iter().any(|(kept, _)| {
                                        (kept.frequency_offset - item.frequency_offset).abs()
                                            < pancetta_qso::MIN_TX_SEPARATION_HZ
                                    });
                                    if too_close {
                                        warn!(
                                            target: "pancetta::tx.policy",
                                            "Multi-TX item (qso_id={:?}) at {:.0} Hz is within \
                                             {:.0} Hz of an already-bundled item; excluding it \
                                             from this bundle instead of letting \
                                             modulate_multi_tx fail the whole bundle — it will \
                                             be sent individually",
                                            item.qso_id,
                                            item.frequency_offset,
                                            pancetta_qso::MIN_TX_SEPARATION_HZ
                                        );
                                        excluded.push((item, p));
                                    } else {
                                        freq_checked.push((item, p));
                                    }
                                }
                                let bundled = freq_checked;

                                // Excluded items still transmit — individually, each on
                                // its own (disagreeing) parity — via the same single-TX
                                // path used for the `tx_items.len() == 1` case above.
                                for (item, p) in excluded {
                                    if dry_run {
                                        info!(
                                            target: "autonomous.dry_run",
                                            "DRY RUN: would have transmitted '{}' at offset {:.0} Hz \
                                             (qso_id={:?}, parity={:?}) [excluded from bundle: parity conflict]",
                                            item.message_text,
                                            item.frequency_offset,
                                            item.qso_id,
                                            p
                                        );
                                    } else {
                                        let msg = ComponentMessage::new(
                                            ComponentId::Autonomous,
                                            ComponentId::Ft8Transmitter,
                                            MessageType::TransmitRequest {
                                                message_text: item.message_text,
                                                frequency_offset: item.frequency_offset,
                                                qso_id: item.qso_id,
                                                tx_parity: p,
                                                origin: crate::message_bus::TxOrigin::Local,
                                            },
                                            Instant::now(),
                                        );
                                        if let Err(e) = message_bus.send_message(msg).await {
                                            warn!(
                                                "Failed to send TransmitRequest for parity-excluded item: {}",
                                                e
                                            );
                                        }
                                    }
                                }

                                let items: Vec<_> = bundled.into_iter().map(|(it, _)| it).collect();
                                if dry_run {
                                    info!(
                                        target: "autonomous.dry_run",
                                        "DRY RUN: would have bundled {} TX items (parity={:?})",
                                        items.len(),
                                        bundle_parity
                                    );
                                    for item in &items {
                                        info!(
                                            target: "autonomous.dry_run",
                                            "DRY RUN:   - '{}' at offset {:.0} Hz (qso_id={:?})",
                                            item.message_text,
                                            item.frequency_offset,
                                            item.qso_id
                                        );
                                    }
                                } else {
                                    info!("Bundling {} TX items into MultiTransmitRequest", items.len());
                                    let msg = ComponentMessage::new(
                                        ComponentId::Autonomous,
                                        ComponentId::Ft8Transmitter,
                                        MessageType::MultiTransmitRequest {
                                            items,
                                            tx_parity: bundle_parity,
                                            origin: crate::message_bus::TxOrigin::Local,
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        warn!("Failed to send MultiTransmitRequest: {}", e);
                                    }
                                }
                            }
                            } // end `if auto_config_enabled` decision/dispatch gate (FQ-F9)
                        }

                        _ = async {
                            loop {
                                match auto_rx.try_recv() {
                                    Ok(message) => {
                                        match message.message_type {
                                            MessageType::DecodedMessage(decoded_msg) => {
                                                slot_messages.push(pancetta_qso::DecodedMessageInfo {
                                                    callsign: decoded_msg.message.from_callsign.clone(),
                                                    frequency_hz: decoded_msg.frequency_offset,
                                                    snr: decoded_msg.snr_db as i32,
                                                    message_text: decoded_msg.text.clone(),
                                                    slot_parity: decoded_msg.slot_parity,
                                                    // hb-103 (Batch 32): plumb through for the
                                                    // content-score TX gate in autonomous.decide().
                                                    confidence: Some(decoded_msg.confidence),
                                                    time_offset_s: Some(decoded_msg.time_offset),
                                                    // hb-247 (Batch 81): v3 lateness term source.
                                                    decode_origin: decoded_msg
                                                        .confidence_features
                                                        .as_ref()
                                                        .and_then(|c| c.decode_origin),
                                                });
                                            }
                                            // PAN-38: a dispatched self-CQ
                                            // failed downstream (radio/CAT
                                            // error, subsystem race) after
                                            // every pre-dispatch gate had
                                            // already permitted it — no QSO
                                            // was actually opened, so roll
                                            // back exactly this attempt's
                                            // speculative streak/offset
                                            // mutations. A no-op if a later
                                            // self-CQ attempt has already
                                            // superseded the snapshot.
                                            MessageType::QsoMessage(
                                                crate::message_bus::QsoMessage::AutonomousCqDispatchFailed {
                                                    attempt_id,
                                                },
                                            ) => {
                                                let mut op = operator.lock().await;
                                                op.restore_cq_state_for_attempt(attempt_id);
                                                drop(op);
                                                // PAN-38 round 2: since
                                                // AutonomousCqOpened is now
                                                // sent BEFORE dispatch
                                                // (round 2), a synchronous
                                                // start_cq_with_id failure
                                                // (or a cross-parity
                                                // deferral before dispatch)
                                                // can leave a
                                                // pending_self_cq_qsos entry
                                                // for a QSO that was never
                                                // actually created — no
                                                // TransmitComplete will ever
                                                // arrive to remove it.
                                                // Linear scan-and-remove by
                                                // value is fine: this map
                                                // holds at most one or two
                                                // entries in practice (a
                                                // self-CQ only fires when
                                                // idle).
                                                pending_self_cq_qsos
                                                    .retain(|_, id| *id != attempt_id);
                                                // PAN-43/45 follow-up (Codex
                                                // round 3 on PR #342): mirrors
                                                // the queue-independent
                                                // fallback drain's own
                                                // tombstone recording (see its
                                                // doc comment) -- kept in sync
                                                // for defensiveness even
                                                // though pure message-bus
                                                // ordering already protects
                                                // this specific path today.
                                                record_rolled_back_attempt_tombstone(
                                                    &mut rolled_back_attempt_tombstones,
                                                    attempt_id,
                                                );
                                            }
                                            // PAN-38 round 1: record the
                                            // qso_id <-> attempt_id link for a
                                            // self-CQ whose `start_cq`
                                            // succeeded, so a later
                                            // TransmitComplete failure for
                                            // this exact QSO can be
                                            // correlated back to it below.
                                            MessageType::QsoMessage(
                                                crate::message_bus::QsoMessage::AutonomousCqOpened {
                                                    qso_id,
                                                    attempt_id,
                                                },
                                            ) => {
                                                // PAN-43/45 follow-up (Codex
                                                // round 3 on PR #342): the
                                                // queue-independent fallback
                                                // rollback can process THIS
                                                // exact attempt_id before this
                                                // Open message -- still
                                                // sitting queued at that point
                                                // -- ever gets processed. That
                                                // rollback already fully
                                                // undid the attempt; inserting
                                                // now would create an entry
                                                // for a QSO that was never
                                                // actually kept alive, which
                                                // no TransmitComplete would
                                                // ever arrive to remove.
                                                if !consume_rolled_back_attempt_tombstone(
                                                    &mut rolled_back_attempt_tombstones,
                                                    attempt_id,
                                                ) {
                                                    pending_self_cq_qsos.insert(qso_id, attempt_id);
                                                }
                                            }
                                            // PAN-38 round 1 (Codex): `start_cq`
                                            // succeeding only means the QSO
                                            // object was created and its
                                            // opening MessageToSend handed to
                                            // the TX worker -- the actual
                                            // radio/CAT transmission can still
                                            // fail, reported here. If it
                                            // belongs to a still-pending
                                            // self-CQ, roll back the same way
                                            // a synchronous start_cq failure
                                            // already does. Every completion
                                            // (success or failure) for a
                                            // tracked qso_id clears the entry
                                            // either way, so this map never
                                            // grows unbounded.
                                            MessageType::TransmitComplete {
                                                success,
                                                qso_id: Some(qso_id),
                                                ..
                                            } => {
                                                if let Some(attempt_id) =
                                                    pending_self_cq_qsos.remove(&qso_id)
                                                {
                                                    if !success {
                                                        let mut op = operator.lock().await;
                                                        op.restore_cq_state_for_attempt(attempt_id);
                                                    }
                                                }
                                            }
                                            _ => {}
                                        }
                                    }
                                    Err(crossbeam_channel::TryRecvError::Empty) => {
                                        tokio::task::yield_now().await;
                                        break;
                                    }
                                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                                }
                            }
                        } => {}
                    }

                    if shutdown.load(Ordering::Acquire) {
                        break;
                    }
                }

                info!("Autonomous operator stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::Autonomous, auto_handle));
        info!("Autonomous operator component started");
        Ok(())
    }
}

#[cfg(test)]
mod parked_score_in_slices_tests {
    use super::*;
    use pancetta_qso::frequency::{FrequencyCandidate, PlacementSnapshot};

    fn candidate(offset_hz: f64, score: f64) -> FrequencyCandidate {
        FrequencyCandidate {
            offset_hz,
            score,
            clear_both_slots: true,
            clear_first: true,
            clear_second: true,
            noise_floor: 0.0,
        }
    }

    #[test]
    fn unparked_sentinel_is_none() {
        let snap = PlacementSnapshot {
            slices: vec![candidate(920.0, 95.0)],
            openness: vec![],
            bin_hz: 25.0,
            range: (200.0, 2700.0),
        };
        assert_eq!(parked_score_in_slices(0, &snap), None);
    }

    #[test]
    fn no_candidate_within_half_bin_width_is_none() {
        let snap = PlacementSnapshot {
            slices: vec![candidate(920.0, 95.0)],
            openness: vec![],
            bin_hz: 25.0,
            range: (200.0, 2700.0),
        };
        // 950 - 920 = 30 Hz, outside the 12.5 Hz half-bin-width window.
        assert_eq!(parked_score_in_slices(950, &snap), None);
    }

    /// Issue #98 item 2: with two candidates both within the half-bin-width
    /// window of the parked frequency, `min_by`-nearest must return the
    /// CLOSER one's score, not whichever happens to be first in `slices`.
    #[test]
    fn picks_nearest_candidate_not_first_match() {
        let snap = PlacementSnapshot {
            slices: vec![
                // Farther from 920 (10 Hz away), but listed FIRST.
                candidate(910.0, 50.0),
                // Closer to 920 (5 Hz away), listed second.
                candidate(915.0, 99.0),
            ],
            openness: vec![],
            bin_hz: 25.0,
            range: (200.0, 2700.0),
        };
        assert_eq!(parked_score_in_slices(920, &snap), Some(99.0));
    }
}

#[cfg(test)]
mod parked_bin_coverage_tests {
    use super::*;
    use pancetta_qso::frequency::PlacementSnapshot;

    #[test]
    fn parked_bin_coverage_maps_hz_to_bin() {
        let snap = PlacementSnapshot {
            slices: vec![],
            openness: vec![3, 1, 0],
            bin_hz: 25.0,
            range: (200.0, 275.0),
        };
        assert_eq!(parked_bin_coverage(0, &snap), None, "0 = unparked sentinel");
        assert_eq!(parked_bin_coverage(225, &snap), Some(1));
    }

    #[test]
    fn parked_bin_coverage_out_of_range_is_none() {
        let snap = PlacementSnapshot {
            slices: vec![],
            openness: vec![3, 1, 0],
            bin_hz: 25.0,
            range: (200.0, 275.0),
        };
        assert_eq!(
            parked_bin_coverage(1000, &snap),
            None,
            "bin index past the end of openness"
        );
    }

    /// Issue #97 item 3: before sharing `pancetta_core::freq_bin`, this
    /// truncated (never floored+clamped) and landed one bin past the end at
    /// the exact top edge, returning `None` where the TUI's own lookup
    /// (`bin_index_for_freq`) resolved the last bin. Now both sides agree.
    #[test]
    fn parked_bin_coverage_top_edge_resolves_last_bin() {
        let snap = PlacementSnapshot {
            slices: vec![],
            openness: vec![3, 1, 0],
            bin_hz: 25.0,
            range: (200.0, 275.0),
        };
        assert_eq!(parked_bin_coverage(275, &snap), Some(0));
    }
}

#[cfg(test)]
mod should_repark_tests {
    use super::*;

    #[test]
    fn repark_gates() {
        let best = pancetta_qso::frequency::FrequencyCandidate {
            offset_hz: 920.0,
            score: 95.0,
            clear_both_slots: true,
            clear_first: true,
            clear_second: true,
            noise_floor: 0.0,
        };
        // disabled → never
        assert_eq!(
            should_repark(false, 0, 1500, Some(0), Some(10.0), Some(&best), 20.0),
            None
        );
        // active QSO → never (LIVE STREAM SAFETY)
        assert_eq!(
            should_repark(true, 1, 1500, Some(0), Some(10.0), Some(&best), 20.0),
            None
        );
        // parked slice still usable (code 2) → hold (hysteresis)
        assert_eq!(
            should_repark(true, 0, 1500, Some(2), Some(60.0), Some(&best), 20.0),
            None
        );
        // busy-both + big gain → repark
        assert_eq!(
            should_repark(true, 0, 1500, Some(0), Some(10.0), Some(&best), 20.0),
            Some(920)
        );
        // busy-both but marginal gain → hold
        assert_eq!(
            should_repark(true, 0, 1500, Some(0), Some(80.0), Some(&best), 20.0),
            None
        );
    }
}

#[cfg(test)]
mod resolve_repark_active_qsos_tests {
    use super::*;
    use std::sync::RwLock;

    #[test]
    fn ok_read_passes_through_the_count() {
        assert_eq!(resolve_repark_active_qsos::<()>(Ok(0)), 0);
        assert_eq!(resolve_repark_active_qsos::<()>(Ok(3)), 3);
    }

    #[test]
    fn err_read_fails_closed_to_max_not_zero() {
        // A poisoned lock must be treated as "assume active" (skip repark
        // this tick), NOT "assume zero" (which would let the LIVE STREAM
        // SAFETY gate proceed as if idle under genuine uncertainty).
        let resolved = resolve_repark_active_qsos::<()>(Err(()));
        assert_eq!(resolved, usize::MAX);
        assert_ne!(resolved, 0, "must not fail open to zero");
    }

    /// End-to-end proof against a REAL poisoned `std::sync::RwLock`, the
    /// exact type `active_tx_qsos` uses — not just the trivial
    /// `Result::unwrap_or` semantics above.
    #[test]
    fn real_poisoned_lock_fails_closed_and_blocks_repark() {
        let lock: std::sync::Arc<RwLock<std::collections::HashSet<String>>> =
            std::sync::Arc::new(RwLock::new(std::collections::HashSet::new()));

        // Poison the lock by panicking while holding the write guard.
        {
            let lock2 = lock.clone();
            let _ = std::thread::spawn(move || {
                let _guard = lock2.write().unwrap();
                panic!("intentionally poisoning the lock for this test");
            })
            .join();
        }
        assert!(lock.is_poisoned(), "lock should be poisoned by the panic");

        let active = resolve_repark_active_qsos(lock.read().map(|s| s.len()));
        assert_eq!(
            active,
            usize::MAX,
            "poisoned lock must resolve to the fail-closed sentinel"
        );

        // Feed straight into should_repark: even with every OTHER condition
        // satisfied (enabled, parked, busy-both coverage, a strong best
        // candidate), the fail-closed active count must hold the repark.
        let best = pancetta_qso::frequency::FrequencyCandidate {
            offset_hz: 920.0,
            score: 95.0,
            clear_both_slots: true,
            clear_first: true,
            clear_second: true,
            noise_floor: 0.0,
        };
        assert_eq!(
            should_repark(true, active, 1500, Some(0), Some(10.0), Some(&best), 20.0),
            None,
            "poisoned-lock read must skip the repark this tick"
        );
    }
}

#[cfg(test)]
mod classify_autonomous_opening_tests {
    use super::*;
    use pancetta_qso::DecodedMessageInfo;

    fn decode(callsign: &str, freq: f64, parity: SlotParity) -> DecodedMessageInfo {
        DecodedMessageInfo {
            callsign: Some(callsign.to_string()),
            frequency_hz: freq,
            snr: -10,
            message_text: format!("CQ {callsign} EM10"),
            slot_parity: Some(parity),
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }
    }

    #[test]
    fn cq_opening_uses_our_offset_and_parity() {
        let (callsign, freq, parity) =
            classify_autonomous_opening("CQ K5ARH EM10", 1234.0, Some(SlotParity::Even), &[]);
        assert_eq!(callsign, None, "calling CQ → no DX callsign");
        assert_eq!(freq, 1234.0, "CQ uses our chosen offset");
        assert_eq!(parity, Some(SlotParity::Even), "CQ uses our TX parity");
    }

    #[test]
    fn pounce_answers_on_dx_decoded_frequency_and_parity() {
        // DX VB7F was decoded at 1500 Hz, Odd slot. The operator chose a TX
        // offset of 600 Hz (which we must NOT use to track the QSO).
        let decodes = [decode("VB7F", 1500.0, SlotParity::Odd)];
        let (callsign, freq, parity) =
            classify_autonomous_opening("VB7F K5ARH EM10", 600.0, Some(SlotParity::Even), &decodes);
        assert_eq!(callsign.as_deref(), Some("VB7F"));
        assert_eq!(freq, 1500.0, "answer Tx=Rx on the DX's decoded frequency");
        assert_eq!(
            parity,
            Some(SlotParity::Odd),
            "respond_to_cq wants the DX's slot parity (it latches our tx = opposite)"
        );
    }

    #[test]
    fn pounce_falls_back_when_dx_decode_missing() {
        // No matching decode this slot → use the item's offset, and recover the
        // DX parity from the operator's computed tx_parity (= dx.opposite()).
        let (callsign, freq, parity) =
            classify_autonomous_opening("VB7F K5ARH EM10", 600.0, Some(SlotParity::Even), &[]);
        assert_eq!(callsign.as_deref(), Some("VB7F"));
        assert_eq!(freq, 600.0, "fallback to the operator's chosen offset");
        assert_eq!(
            parity,
            Some(SlotParity::Odd),
            "fallback DX parity = opposite of our computed tx_parity"
        );
    }

    #[test]
    fn pounce_matches_dx_callsign_case_insensitively() {
        let decodes = [decode("vb7f", 1500.0, SlotParity::Odd)];
        let (callsign, freq, _) =
            classify_autonomous_opening("VB7F K5ARH EM10", 600.0, Some(SlotParity::Even), &decodes);
        assert_eq!(callsign.as_deref(), Some("VB7F"));
        assert_eq!(
            freq, 1500.0,
            "case-insensitive sender match still resolves DX freq"
        );
    }
}

#[cfg(test)]
mod plan_slot_transmissions_tests {
    use super::*;
    use crate::message_bus::TransmitRequestItem;
    use pancetta_core::TxPolicy;
    use pancetta_qso::DecodedMessageInfo;

    fn decode(callsign: &str, freq: f64, parity: SlotParity) -> DecodedMessageInfo {
        DecodedMessageInfo {
            callsign: Some(callsign.to_string()),
            frequency_hz: freq,
            snr: -10,
            message_text: format!("CQ {callsign} EM10"),
            slot_parity: Some(parity),
            confidence: None,
            time_offset_s: None,
            decode_origin: None,
        }
    }

    fn opening(text: &str, offset: f64) -> (TransmitRequestItem, Option<SlotParity>) {
        (
            TransmitRequestItem {
                message_text: text.to_string(),
                frequency_offset: offset,
                qso_id: None,
            },
            Some(SlotParity::Even),
        )
    }

    fn in_progress(text: &str, qso_id: &str) -> (TransmitRequestItem, Option<SlotParity>) {
        (
            TransmitRequestItem {
                message_text: text.to_string(),
                frequency_offset: 1500.0,
                qso_id: Some(qso_id.to_string()),
            },
            Some(SlotParity::Odd),
        )
    }

    // --- Runtime gate (Shift+Q) -------------------------------------------

    #[test]
    fn runtime_gate_closed_drops_everything() {
        let items = vec![
            opening("VB7F K5ARH EM10", 600.0),
            in_progress("VB7F K5ARH R-09", "q1"),
        ];
        let plan = plan_slot_transmissions(items, false, TxPolicy::Full, false, &[], true, None);
        assert!(plan.qso_starts.is_empty(), "Shift+Q drops openings");
        assert!(plan.tx_items.is_empty(), "Shift+Q drops in-progress TX too");
        assert_eq!(plan.runtime_gate_dropped, 2);
        assert_eq!(plan.policy_dropped, 0);
    }

    // --- TX policy --------------------------------------------------------

    #[test]
    fn policy_respondonly_drops_openings_keeps_in_progress() {
        let items = vec![
            opening("VB7F K5ARH EM10", 600.0),
            in_progress("VB7F K5ARH R-09", "q1"),
        ];
        let plan =
            plan_slot_transmissions(items, true, TxPolicy::RespondOnly, false, &[], true, None);
        assert!(
            plan.qso_starts.is_empty(),
            "RespondOnly suppresses autonomous initiations (no new QSO opened)"
        );
        assert_eq!(
            plan.policy_dropped, 1,
            "the one opening was the suppressed initiation"
        );
        assert_eq!(
            plan.tx_items.len(),
            1,
            "in-progress (qso_id=Some) item still flows under RespondOnly"
        );
        assert_eq!(plan.tx_items[0].0.qso_id.as_deref(), Some("q1"));
    }

    #[test]
    fn policy_disabled_drops_openings_keeps_in_progress() {
        // Disabled suppresses initiation here too; the hard-mute of in-progress
        // items happens later at the TX worker, not in this planner.
        let items = vec![
            opening("VB7F K5ARH EM10", 600.0),
            in_progress("VB7F K5ARH R-09", "q1"),
        ];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Disabled, false, &[], true, None);
        assert!(plan.qso_starts.is_empty());
        assert_eq!(plan.policy_dropped, 1);
        assert_eq!(plan.tx_items.len(), 1);
    }

    // --- Full policy: opening → QSO-start split ---------------------------

    #[test]
    fn full_policy_pounce_becomes_qso_start_on_dx_freq() {
        let decodes = [decode("VB7F", 1500.0, SlotParity::Odd)];
        let items = vec![opening("VB7F K5ARH EM10", 600.0)];
        let plan =
            plan_slot_transmissions(items, true, TxPolicy::Full, false, &decodes, true, None);
        assert_eq!(plan.qso_starts.len(), 1, "the pounce became a QSO start");
        assert_eq!(plan.qso_starts[0].callsign.as_deref(), Some("VB7F"));
        assert_eq!(
            plan.qso_starts[0].frequency, 1500.0,
            "Tx=Rx on the DX's decoded freq, not the 600 Hz TX offset"
        );
        assert!(
            plan.tx_items.is_empty(),
            "the opening is routed via QSO start, NOT also sent raw (no double-send)"
        );
        assert_eq!(plan.policy_dropped, 0);
        assert_eq!(plan.runtime_gate_dropped, 0);
    }

    #[test]
    fn full_policy_cq_becomes_qso_start_with_no_callsign() {
        let items = vec![opening("CQ K5ARH EM10", 1200.0)];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &[], true, None);
        assert_eq!(plan.qso_starts.len(), 1);
        assert_eq!(
            plan.qso_starts[0].callsign, None,
            "calling CQ → no DX callsign"
        );
        assert_eq!(
            plan.qso_starts[0].frequency, 1200.0,
            "CQ uses our chosen offset"
        );
        assert!(plan.tx_items.is_empty());
    }

    #[test]
    fn cq_attempt_id_attaches_to_self_cq_start_but_not_a_pounce_start() {
        // PAN-38: the self-CQ opening (callsign: None) carries the caller's
        // `self_cq_attempt_id` through to `StartAutonomousQso.cq_attempt_id`
        // so a downstream `start_cq` failure can be traced back to the
        // exact snapshot to roll back. A pounce opening has no CQ-streak
        // state to roll back, so it must never carry one, even when the
        // same slot also produced a self-CQ id.
        let items = vec![opening("CQ K5ARH EM10", 1200.0)];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &[], true, Some(42));
        assert_eq!(plan.qso_starts.len(), 1);
        assert_eq!(plan.qso_starts[0].callsign, None);
        assert_eq!(
            plan.qso_starts[0].cq_attempt_id,
            Some(42),
            "self-CQ start must carry the attempt id"
        );

        let decodes = [decode("VB7F", 1500.0, SlotParity::Odd)];
        let items = vec![opening("VB7F K5ARH EM10", 600.0)];
        let plan =
            plan_slot_transmissions(items, true, TxPolicy::Full, false, &decodes, true, Some(42));
        assert_eq!(plan.qso_starts.len(), 1);
        assert_eq!(plan.qso_starts[0].callsign.as_deref(), Some("VB7F"));
        assert_eq!(
            plan.qso_starts[0].cq_attempt_id, None,
            "a pounce start must never carry a cq_attempt_id"
        );
    }

    #[test]
    fn full_policy_in_progress_item_stays_on_raw_tx_path() {
        let items = vec![in_progress("VB7F K5ARH R-09", "q1")];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &[], true, None);
        assert!(
            plan.qso_starts.is_empty(),
            "a qso_id=Some sequencer item is never a QSO start"
        );
        assert_eq!(
            plan.tx_items.len(),
            1,
            "it stays on the raw TransmitRequest path"
        );
    }

    #[test]
    fn mixed_opening_and_in_progress_split_correctly() {
        let decodes = [decode("VB7F", 1500.0, SlotParity::Odd)];
        let items = vec![
            opening("VB7F K5ARH EM10", 600.0),
            in_progress("W1AW K5ARH R-12", "q7"),
        ];
        let plan =
            plan_slot_transmissions(items, true, TxPolicy::Full, false, &decodes, true, None);
        assert_eq!(plan.qso_starts.len(), 1, "the opening → start");
        assert_eq!(plan.tx_items.len(), 1, "the in-progress item → raw TX");
        assert_eq!(plan.tx_items[0].0.qso_id.as_deref(), Some("q7"));
    }

    // --- operator-presence gate (FCC §97.221) ----------------------------

    #[test]
    fn absent_operator_suppresses_initiation_keeps_in_progress() {
        // Full policy + runtime gate open, but operator NOT present: autonomous
        // initiation (qso_id == None) is dropped; in-progress QSO items flow.
        let items = vec![
            opening("VB7F K5ARH EM10", 600.0),
            in_progress("W1AW K5ARH R-12", "q7"),
        ];
        let plan = plan_slot_transmissions(
            items,
            true,
            TxPolicy::Full,
            false,
            &[],
            /*present*/ false,
            None,
        );
        assert!(
            plan.qso_starts.is_empty(),
            "absent operator: no autonomous initiation"
        );
        assert_eq!(
            plan.presence_dropped, 1,
            "the opening counted as presence-dropped"
        );
        assert_eq!(
            plan.policy_dropped, 0,
            "policy allowed; the gate was presence"
        );
        assert_eq!(plan.tx_items.len(), 1, "in-progress QSO item still flows");
        assert_eq!(plan.tx_items[0].0.qso_id.as_deref(), Some("q7"));
    }

    #[test]
    fn present_operator_allows_initiation_under_full() {
        let decodes = [decode("VB7F", 1500.0, SlotParity::Odd)];
        let items = vec![opening("VB7F K5ARH EM10", 600.0)];
        let plan = plan_slot_transmissions(
            items,
            true,
            TxPolicy::Full,
            false,
            &decodes,
            /*present*/ true,
            None,
        );
        assert_eq!(
            plan.qso_starts.len(),
            1,
            "present operator: initiation allowed"
        );
        assert_eq!(plan.presence_dropped, 0);
    }

    // --- dry_run ----------------------------------------------------------

    #[test]
    fn dry_run_records_openings_without_creating_qsos() {
        let items = vec![
            opening("VB7F K5ARH EM10", 600.0),
            in_progress("W1AW K5ARH R-12", "q7"),
        ];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, true, &[], true, None);
        assert!(plan.qso_starts.is_empty(), "dry_run opens no QSOs");
        assert_eq!(
            plan.dry_run_openings,
            vec!["VB7F K5ARH EM10".to_string()],
            "dry_run records the opening text for logging"
        );
        assert_eq!(
            plan.tx_items.len(),
            1,
            "in-progress items remain for the bundler (which dry-run-logs them)"
        );
    }

    // --- gate ordering: runtime gate wins over policy --------------------

    #[test]
    fn runtime_gate_takes_precedence_over_policy() {
        // Even under Full policy, a closed runtime gate drops everything and
        // nothing is attributed to the policy.
        let items = vec![opening("VB7F K5ARH EM10", 600.0)];
        let plan = plan_slot_transmissions(items, false, TxPolicy::Full, false, &[], true, None);
        assert!(plan.qso_starts.is_empty());
        assert!(plan.tx_items.is_empty());
        assert_eq!(plan.runtime_gate_dropped, 1);
        assert_eq!(
            plan.policy_dropped, 0,
            "runtime gate already cleared the list"
        );
    }
}

#[cfg(test)]
mod self_cq_suppressed_tests {
    use super::*;
    use pancetta_core::TxPolicy;

    #[test]
    fn no_self_cq_this_cycle_never_suppressed() {
        assert!(!self_cq_suppressed(
            false,
            true,
            TxPolicy::Full,
            true,
            false
        ));
    }

    #[test]
    fn self_cq_with_all_gates_open_is_not_suppressed() {
        assert!(!self_cq_suppressed(true, true, TxPolicy::Full, true, false));
    }

    #[test]
    fn runtime_gate_closed_suppresses_self_cq() {
        assert!(self_cq_suppressed(true, false, TxPolicy::Full, true, false));
    }

    #[test]
    fn policy_disallowing_initiation_suppresses_self_cq() {
        assert!(self_cq_suppressed(
            true,
            true,
            TxPolicy::RespondOnly,
            true,
            false
        ));
    }

    #[test]
    fn operator_absent_suppresses_self_cq() {
        assert!(self_cq_suppressed(true, true, TxPolicy::Full, false, false));
    }

    #[test]
    fn dry_run_suppresses_self_cq() {
        assert!(self_cq_suppressed(true, true, TxPolicy::Full, true, true));
    }
}

#[cfg(test)]
mod drain_pending_autonomous_cq_dispatch_failures_tests {
    use super::*;

    #[allow(clippy::field_reassign_with_default)]
    fn operator_with_one_real_self_cq_dispatched() -> (pancetta_qso::AutonomousOperator, u64) {
        let mut config = pancetta_qso::AutonomousConfig::default();
        config.enabled = true;
        config.slot_parity = pancetta_qso::SlotParityConfig::Even;
        config.cq_after_idle_cycles = 2;
        config.listen_cycle.initial_interval = 100;
        let mut op =
            pancetta_qso::AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.update_spectral(pancetta_qso::frequency::SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });
        op.decide_at(0); // idle
        op.decide_at(0); // CQ -- takes a real snapshot
        let attempt_id = op
            .last_cq_attempt_id()
            .expect("a self-CQ ran this cycle and took a snapshot");
        (op, attempt_id)
    }

    /// PAN-43/45 follow-up (Codex round 2 on PR #342) regression: before
    /// this fix, an `attempt_id` that never got through the QSO task's
    /// message-bus retries was lost entirely -- nothing ever drained
    /// `pending_autonomous_cq_dispatch_failures` into a real rollback.
    /// `last_cq_attempt_id()` going from `Some` to `None` is the
    /// observable proof that `restore_cq_state_for_attempt` (a private
    /// `pancetta-qso` method, not directly assertable from this crate)
    /// actually ran for this exact attempt, via its `.take()` on the
    /// matching snapshot.
    #[test]
    fn drains_a_pending_failure_into_a_real_rollback() {
        let (mut op, attempt_id) = operator_with_one_real_self_cq_dispatched();
        let mut pending_self_cq_qsos: std::collections::HashMap<String, u64> =
            std::collections::HashMap::from([("some-qso-id".to_string(), attempt_id)]);
        let pending_failures = std::sync::Mutex::new(vec![attempt_id]);
        let mut tombstones = std::collections::VecDeque::new();

        drain_pending_autonomous_cq_dispatch_failures(
            &mut op,
            &mut pending_self_cq_qsos,
            &pending_failures,
            &mut tombstones,
        );

        assert!(
            op.last_cq_attempt_id().is_none(),
            "the drain must actually invoke restore_cq_state_for_attempt for this attempt, \
             not just clear the pending list"
        );
        assert!(
            pending_failures.lock().unwrap().is_empty(),
            "a drained failure must be removed from the pending list, not reprocessed forever"
        );
        assert!(
            pending_self_cq_qsos.is_empty(),
            "the matching pending_self_cq_qsos entry must be cleaned up too, mirroring the \
             message-driven AutonomousCqDispatchFailed handler"
        );
        assert_eq!(
            tombstones.into_iter().collect::<Vec<_>>(),
            vec![attempt_id],
            "a drained rollback must record a tombstone for a same-attempt Open that's still \
             queued on the bus"
        );
    }

    #[test]
    fn an_empty_pending_list_is_a_complete_no_op() {
        let (mut op, attempt_id) = operator_with_one_real_self_cq_dispatched();
        let mut pending_self_cq_qsos: std::collections::HashMap<String, u64> =
            std::collections::HashMap::new();
        let pending_failures = std::sync::Mutex::new(Vec::new());
        let mut tombstones = std::collections::VecDeque::new();

        drain_pending_autonomous_cq_dispatch_failures(
            &mut op,
            &mut pending_self_cq_qsos,
            &pending_failures,
            &mut tombstones,
        );

        assert_eq!(
            op.last_cq_attempt_id(),
            Some(attempt_id),
            "nothing pending must mean nothing is touched"
        );
        assert!(tombstones.is_empty());
    }
}

/// An empty `active_tx_offsets` snapshot for drain tests whose assertions
/// are about the QSO-side mutation rather than the map mirror. The drain
/// only rewrites keys that already exist (see its doc comment), so an empty
/// map makes the mirror a deliberate no-op.
#[cfg(test)]
fn no_offsets() -> std::sync::RwLock<std::collections::HashMap<String, f64>> {
    std::sync::RwLock::new(std::collections::HashMap::new())
}

/// The shared Hold/Auto atomic `drain_pending_qso_offset_requests` re-reads
/// at commit time (PAN-72, Codex round 1 on PR #350, finding 6). Auto means
/// queued actions are allowed to commit; see `hold_mode()` in
/// `drain_pending_qso_offset_requests_tests` for the discard case.
#[cfg(test)]
fn drain_auto_mode() -> std::sync::atomic::AtomicU8 {
    std::sync::atomic::AtomicU8::new(pancetta_core::TxFreqMode::Auto.as_u8())
}

#[cfg(test)]
mod drain_pending_qso_offset_requests_tests {
    use super::*;

    /// Minimal manager config, mirroring `qso.rs`'s `replay_local_log_tests::manager()`.
    fn manager() -> pancetta_qso::QsoManager {
        pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
            our_callsign: "W1ABC".to_string(),
            our_grid: Some("FN42".to_string()),
            ..Default::default()
        })
    }

    /// An operator with the smart allocator actually live (Auto mode +
    /// spectral data), mirroring `pancetta-qso`'s own
    /// `allocate_smart_frequency_avoids_the_given_offset` test -- this
    /// operator's `tx_freq_mode` is its OWN internal atomic (set via
    /// `set_tx_freq_mode_source`), unrelated to any `QsoManager`'s
    /// `tx_freq_mode` source, so none of Task 8's `qso_manager_for_supervisor`
    /// disconnected-Arc caveat applies here.
    #[allow(clippy::field_reassign_with_default)]
    fn operator_with_live_allocator() -> pancetta_qso::AutonomousOperator {
        let mut config = pancetta_qso::AutonomousConfig::default();
        config.enabled = true;
        let mut op =
            pancetta_qso::AutonomousOperator::new(config, "W1ABC".into(), Some("FN42".into()));
        op.set_tx_freq_mode_source(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            pancetta_core::TxFreqMode::Auto.as_u8(),
        )));
        op.update_spectral(pancetta_qso::frequency::SpectralSnapshot {
            power_bins: vec![0.0f32; 140],
            freq_min_hz: 200.0,
            freq_max_hz: 2800.0,
        });
        op
    }

    use super::drain_auto_mode as auto_mode;

    /// Hold = every queued action is discarded at drain time, whatever the
    /// mode was when it was enqueued.
    fn hold_mode() -> std::sync::atomic::AtomicU8 {
        std::sync::atomic::AtomicU8::new(pancetta_core::TxFreqMode::Hold.as_u8())
    }

    #[tokio::test]
    async fn switch_action_resolves_via_allocator_and_commits() {
        let mut op = operator_with_live_allocator();
        let qso_manager = manager();
        let qso_id = qso_manager
            .start_cq(1500.0, None, false)
            .await
            .expect("start_cq should succeed");
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                qso_id,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz: 1500.0 },
            ),
        ]);

        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &no_offsets(),
            &auto_mode(),
        )
        .await;

        assert!(
            pending.lock().unwrap().is_empty(),
            "a drained request must be removed from the pending list"
        );
        let (_, progress) = qso_manager
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(id, _)| *id == qso_id)
            .unwrap();
        assert!(
            (progress.metadata.frequency - 1500.0).abs() > f64::EPSILON,
            "Switch must resolve to something other than the avoided 1500 Hz via the allocator, \
             got {}",
            progress.metadata.frequency
        );
    }

    #[tokio::test]
    async fn revert_action_uses_target_hz_directly_no_allocator_call() {
        // Deliberately Hold mode / no spectral data -- if `Revert` ever
        // routed through the allocator instead of using `target_hz`
        // directly, `allocate_smart_frequency` would fall back to
        // `config.tx_offset_hz` (default 1000.0), not land on 1200.0.
        let mut op = pancetta_qso::AutonomousOperator::new(
            pancetta_qso::AutonomousConfig::default(),
            "W1ABC".into(),
            Some("FN42".into()),
        );
        let qso_manager = manager();
        let qso_id = qso_manager
            .start_cq(1500.0, None, false)
            .await
            .expect("start_cq should succeed");
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                qso_id,
                pancetta_qso::qso_manager::OffsetAction::Revert { target_hz: 1200.0 },
            ),
        ]);

        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &no_offsets(),
            &auto_mode(),
        )
        .await;

        assert!(pending.lock().unwrap().is_empty());
        let (_, progress) = qso_manager
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(id, _)| *id == qso_id)
            .unwrap();
        assert_eq!(
            progress.metadata.frequency, 1200.0,
            "Revert must land on target_hz exactly, not any allocator-derived value"
        );
    }

    #[tokio::test]
    async fn an_empty_pending_list_is_a_complete_no_op() {
        let mut op = operator_with_live_allocator();
        let qso_manager = manager();
        let pending = std::sync::Mutex::new(Vec::new());

        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &no_offsets(),
            &auto_mode(),
        )
        .await;

        assert!(pending.lock().unwrap().is_empty());
    }

    /// PAN-72 final review (finding 4): the drain must mirror the applied
    /// offset into `active_tx_offsets`.
    ///
    /// `apply_tx_offset_switch` does emit `QsoEvent::TxOffsetApplied`, but
    /// only to trigger a UI-snapshot refresh — its handler never touches this
    /// map, which is otherwise only refreshed on a `StateChanged` carrying a
    /// frequency — so without this write the map keeps the PRE-switch offset
    /// until the QSO's next state transition. The allocator reads the map
    /// every tick through
    /// `set_own_frequencies`, so a stale entry both guards a slot the QSO has
    /// already vacated and leaves the allocator blind to where the QSO really
    /// is; the next `u` nudge's `avoid_hz` (also read from this map) would
    /// name the wrong frequency too.
    #[tokio::test]
    async fn a_committed_switch_refreshes_the_active_tx_offsets_entry() {
        let mut op = operator_with_live_allocator();
        let qso_manager = manager();
        let qso_id = qso_manager
            .start_cq(1500.0, None, false)
            .await
            .expect("start_cq should succeed");
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        // Seed the map the way the real event-forwarder does on StateChanged.
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key.clone(), 1500.0)]));
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                qso_id,
                pancetta_qso::qso_manager::OffsetAction::Revert { target_hz: 1200.0 },
            ),
        ]);

        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &active_tx_offsets,
            &auto_mode(),
        )
        .await;

        assert_eq!(
            active_tx_offsets.read().unwrap().get(&key).copied(),
            Some(1200.0),
            "active_tx_offsets must reflect the offset the QSO actually moved to"
        );
    }

    /// The mirror must not resurrect a key the forwarder already purged
    /// (terminal-`Failed` / completion grace expiry). A late request for such
    /// a QSO still commits QSO-side, but must not re-admit it to the
    /// TX-active own-frequency map.
    #[tokio::test]
    async fn the_mirror_never_inserts_a_key_the_forwarder_has_not_admitted() {
        let mut op = operator_with_live_allocator();
        let qso_manager = manager();
        let qso_id = qso_manager
            .start_cq(1500.0, None, false)
            .await
            .expect("start_cq should succeed");
        let active_tx_offsets = no_offsets();
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                qso_id,
                pancetta_qso::qso_manager::OffsetAction::Revert { target_hz: 1200.0 },
            ),
        ]);

        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &active_tx_offsets,
            &auto_mode(),
        )
        .await;

        assert!(
            active_tx_offsets.read().unwrap().is_empty(),
            "an absent key means the forwarder does not consider this QSO \
             TX-active; the drain must not insert one"
        );
    }

    /// The mirror stores the offset `apply_tx_offset_switch` actually applied
    /// (post-clamp), not the raw request — an out-of-band `Revert` target
    /// must leave the map and the QSO agreeing with each other.
    #[tokio::test]
    async fn the_mirror_stores_the_clamped_offset_not_the_raw_request() {
        let mut op = operator_with_live_allocator();
        let qso_manager = manager();
        let qso_id = qso_manager
            .start_cq(1500.0, None, false)
            .await
            .expect("start_cq should succeed");
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key.clone(), 1500.0)]));
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                qso_id,
                pancetta_qso::qso_manager::OffsetAction::Revert { target_hz: 9000.0 },
            ),
        ]);

        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &active_tx_offsets,
            &auto_mode(),
        )
        .await;

        let (_, progress) = qso_manager
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(id, _)| *id == qso_id)
            .unwrap();
        assert_eq!(
            progress.metadata.frequency,
            pancetta_qso::qso_manager::ACTIVE_QSO_TX_OFFSET_MAX_HZ
        );
        assert_eq!(
            active_tx_offsets.read().unwrap().get(&key).copied(),
            Some(pancetta_qso::qso_manager::ACTIVE_QSO_TX_OFFSET_MAX_HZ),
            "the map must agree with the QSO after clamping"
        );
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 6): the operator can press
    /// `f` for Hold after an action is queued but before this once-per-slot
    /// drain runs. Committing anyway breaks Hold's promise — a `Switch`
    /// resolves through the allocator's Hold early return onto the PARKED
    /// offset, and a `Revert` does not consult the allocator at all.
    #[tokio::test]
    async fn hold_mode_at_drain_time_discards_every_queued_action() {
        let mut op = operator_with_live_allocator();
        let qso_manager = manager();
        let switching = qso_manager.start_cq(1500.0, None, false).await.unwrap();
        let reverting = qso_manager.start_cq(1800.0, None, false).await.unwrap();
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                switching,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz: 1500.0 },
            ),
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                reverting,
                pancetta_qso::qso_manager::OffsetAction::Revert { target_hz: 700.0 },
            ),
        ]);

        // Enqueued under Auto (as the stall detector requires), drained under
        // Hold — the exact race the operator's `f` press opens.
        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &no_offsets(),
            &hold_mode(),
        )
        .await;

        assert!(
            pending.lock().unwrap().is_empty(),
            "discarded actions must still be consumed off the mailbox, not \
             left to fire on a later tick"
        );
        let offsets: std::collections::HashMap<_, _> = qso_manager
            .get_active_qsos()
            .await
            .into_iter()
            .map(|(id, p)| (id, p.metadata.frequency))
            .collect();
        assert_eq!(
            offsets.get(&switching),
            Some(&1500.0),
            "Hold must leave a queued Switch's QSO exactly where it is"
        );
        assert_eq!(
            offsets.get(&reverting),
            Some(&1800.0),
            "Hold must leave a queued Revert's QSO exactly where it is"
        );
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 1): two QSOs switching in
    /// the same drain batch must not land on the same offset.
    ///
    /// Every `Switch` in one batch ranks against the identical allocator
    /// occupancy view — `set_own_frequencies` only syncs from
    /// `active_tx_offsets` LATER in the same tick — and each request only
    /// excludes its OWN original offset, so without a batch-local reservation
    /// both can pick the same best candidate and collapse two TX streams onto
    /// one frequency.
    #[tokio::test]
    async fn concurrent_switches_in_one_batch_never_collapse_onto_one_offset() {
        let mut op = operator_with_live_allocator();
        let qso_manager = manager();
        let first = qso_manager.start_cq(1500.0, None, false).await.unwrap();
        let second = qso_manager.start_cq(1520.0, None, false).await.unwrap();
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                first,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz: 1500.0 },
            ),
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                second,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz: 1520.0 },
            ),
        ]);

        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &no_offsets(),
            &auto_mode(),
        )
        .await;

        let offsets: std::collections::HashMap<_, _> = qso_manager
            .get_active_qsos()
            .await
            .into_iter()
            .map(|(id, p)| (id, p.metadata.frequency))
            .collect();
        let a = *offsets.get(&first).expect("first QSO still active");
        let b = *offsets.get(&second).expect("second QSO still active");
        assert_ne!(
            a, b,
            "two concurrent switches resolved in one batch must not land on \
             the same TX offset ({a} Hz)"
        );
    }

    #[tokio::test]
    async fn a_request_for_a_since_removed_qso_is_logged_and_skipped_not_propagated() {
        let mut op = operator_with_live_allocator();
        let qso_manager = manager();
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                uuid::Uuid::new_v4(),
                pancetta_qso::qso_manager::OffsetAction::Revert { target_hz: 1200.0 },
            ),
        ]);

        // Must not panic even though the qso_id doesn't exist in the manager.
        drain_pending_qso_offset_requests(
            &mut op,
            &qso_manager,
            &pending,
            &no_offsets(),
            &auto_mode(),
        )
        .await;

        assert!(pending.lock().unwrap().is_empty());
    }
}

/// PAN-72 task-review Critical-finding regression coverage: `self
/// .qso_manager_watch.subscribe()` (autonomous.rs spawn setup) +
/// `.borrow().clone()` fresh every tick, instead of the pre-fix plain
/// `self.qso_manager_for_supervisor.clone()` captured ONCE at
/// Autonomous-task spawn time. The bug lived entirely in "which
/// `QsoManager` reference the tick loop uses to call
/// `drain_pending_qso_offset_requests`" -- that function itself always
/// correctly operates on whatever manager it's handed (proved by
/// `drain_pending_qso_offset_requests_tests` above). Driving this through
/// the REAL spawned `tokio::spawn` tick loop + `health.rs`'s real
/// component-restart supervisor + `operator.lock().await` machinery would
/// need a live coordinator, a real Qso-component panic/restart cycle, and
/// slot-interval timing -- disproportionate infrastructure for what's
/// fundamentally a two-line reference-selection bug. Instead, these tests
/// exercise the handle-refresh mechanism itself (the `watch` subscribe +
/// fresh-borrow pattern) directly, using `drain_pending_qso_offset_requests`
/// as an oracle to make the difference observable and not just a
/// type-level clone.
#[cfg(test)]
mod qso_manager_watch_refresh_tests {
    use super::*;

    fn manager() -> pancetta_qso::QsoManager {
        pancetta_qso::QsoManager::new(pancetta_qso::QsoManagerConfig {
            our_callsign: "W1ABC".to_string(),
            our_grid: Some("FN42".to_string()),
            ..Default::default()
        })
    }

    fn hold_mode_operator() -> pancetta_qso::AutonomousOperator {
        pancetta_qso::AutonomousOperator::new(
            pancetta_qso::AutonomousConfig::default(),
            "W1ABC".into(),
            Some("FN42".into()),
        )
    }

    #[tokio::test]
    async fn tick_loop_observes_a_post_spawn_qso_component_restart() {
        // "Before restart": the manager instance that existed when the
        // Autonomous task's `tokio::spawn` closure ran its ONE-TIME
        // `self.qso_manager_watch.subscribe()` capture.
        let manager_before_restart = manager();
        let (watch_tx, _seed_rx) =
            tokio::sync::watch::channel(Some(manager_before_restart.clone()));
        let qso_manager_watch = watch_tx.subscribe();

        // Simulate health.rs restarting ONLY the Qso component:
        // `start_qso_component` builds a brand-new `QsoManager` (a
        // disjoint, empty QSO map) and now also publishes it onto the
        // watch channel -- mirrors the `self.qso_manager_watch.send(...)`
        // call added next to `qso_manager_for_supervisor`'s assignment in
        // qso.rs.
        let manager_after_restart = manager();
        watch_tx
            .send(Some(manager_after_restart.clone()))
            .expect("the subscriber above is still alive");

        // A QSO that exists ONLY on the post-restart manager -- exactly
        // what the QSO event-forwarder (which IS respawned together with
        // the new manager) would push a request for.
        let qso_id = manager_after_restart
            .start_cq(1500.0, None, false)
            .await
            .expect("start_cq should succeed on the new manager");
        assert!(
            manager_before_restart
                .get_active_qsos()
                .await
                .into_iter()
                .all(|(id, _)| id != qso_id),
            "sanity: the pre-restart manager must NOT contain the post-restart QSO"
        );

        // THE FIX: re-borrow the watch channel FRESH, exactly as the tick
        // loop does every cycle -- not a stale spawn-time clone.
        let current = qso_manager_watch
            .borrow()
            .clone()
            .expect("the receiver must observe the post-restart value");

        let mut op = hold_mode_operator();
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                qso_id,
                pancetta_qso::qso_manager::OffsetAction::Revert { target_hz: 1200.0 },
            ),
        ]);
        drain_pending_qso_offset_requests(
            &mut op,
            &current,
            &pending,
            &no_offsets(),
            &drain_auto_mode(),
        )
        .await;

        assert!(
            pending.lock().unwrap().is_empty(),
            "a drained request must be removed from the pending list"
        );
        let (_, progress) = manager_after_restart
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(id, _)| *id == qso_id)
            .expect(
                "the QSO must be found and updated on the NEW manager -- this is exactly the \
                 scenario the pre-fix spawn-time-captured clone would have silently missed \
                 ('QSO likely completed') for the rest of the process's uptime",
            );
        assert_eq!(
            progress.metadata.frequency, 1200.0,
            "the fresh-each-tick watch borrow must resolve against the post-restart manager"
        );
    }

    #[tokio::test]
    async fn stale_pre_restart_clone_would_have_missed_the_request() {
        // Direct proof of the bug this fixes: a plain clone captured ONCE
        // (the pre-fix `qso_manager_for_offset_drain`) never sees a QSO
        // that only exists on the post-restart manager --
        // `apply_tx_offset_switch` errors and the drain logs+skips it,
        // exactly matching the reviewer's described misdiagnosis ("QSO
        // likely completed" when it's actually alive on a different,
        // newer manager).
        let manager_before_restart = manager();
        let manager_after_restart = manager();
        let qso_id = manager_after_restart
            .start_cq(1500.0, None, false)
            .await
            .expect("start_cq should succeed on the new manager");

        let mut op = hold_mode_operator();
        let pending = std::sync::Mutex::new(vec![
            pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                qso_id,
                pancetta_qso::qso_manager::OffsetAction::Revert { target_hz: 1200.0 },
            ),
        ]);

        // Must not panic -- mirrors the drain function's documented "log
        // and skip" behavior for a QSO the handed-in manager doesn't know
        // about.
        drain_pending_qso_offset_requests(
            &mut op,
            &manager_before_restart,
            &pending,
            &no_offsets(),
            &drain_auto_mode(),
        )
        .await;

        assert!(
            pending.lock().unwrap().is_empty(),
            "the request is still consumed off the mailbox even though it silently failed to \
             apply -- this IS the bug: no crash, no test failure, just a permanently missed \
             action"
        );
        let real_progress = manager_after_restart
            .get_active_qsos()
            .await
            .into_iter()
            .find(|(id, _)| *id == qso_id)
            .map(|(_, progress)| progress.metadata.frequency);
        assert_ne!(
            real_progress,
            Some(1200.0),
            "the QSO on the REAL (post-restart) manager must be untouched -- a drain against \
             the stale pre-restart manager has no way to reach it, demonstrating the exact \
             silent-failure this fix closes"
        );
    }
}

#[cfg(test)]
mod rolled_back_attempt_tombstone_tests {
    use super::*;

    #[test]
    fn a_consumed_tombstone_is_removed_and_reports_present() {
        let mut tombstones = std::collections::VecDeque::new();
        record_rolled_back_attempt_tombstone(&mut tombstones, 7);
        assert!(consume_rolled_back_attempt_tombstone(&mut tombstones, 7));
        assert!(
            tombstones.is_empty(),
            "a consumed tombstone must not remain and be consumable again"
        );
    }

    #[test]
    fn consuming_an_absent_attempt_id_reports_absent_and_is_a_no_op() {
        let mut tombstones = std::collections::VecDeque::new();
        record_rolled_back_attempt_tombstone(&mut tombstones, 1);
        assert!(!consume_rolled_back_attempt_tombstone(&mut tombstones, 99));
        assert_eq!(
            tombstones.into_iter().collect::<Vec<_>>(),
            vec![1],
            "consuming a non-matching id must not disturb an unrelated tombstone"
        );
    }

    #[test]
    fn recording_the_same_attempt_id_twice_does_not_duplicate() {
        let mut tombstones = std::collections::VecDeque::new();
        record_rolled_back_attempt_tombstone(&mut tombstones, 3);
        record_rolled_back_attempt_tombstone(&mut tombstones, 3);
        assert_eq!(tombstones.into_iter().collect::<Vec<_>>(), vec![3]);
    }

    /// PAN-43/45 follow-up (Codex round 3 on PR #342) regression: without
    /// a cap, an attempt_id that never gets a matching `AutonomousCqOpened`
    /// at all (PAN-45's registration-declined path -- no Open was ever
    /// sent) would sit in the tombstone list forever, growing it
    /// unboundedly under repeated failures.
    #[test]
    fn recording_past_the_cap_evicts_the_oldest_first() {
        let mut tombstones = std::collections::VecDeque::new();
        for id in 0..(MAX_ROLLED_BACK_ATTEMPT_TOMBSTONES as u64 + 3) {
            record_rolled_back_attempt_tombstone(&mut tombstones, id);
        }
        assert_eq!(
            tombstones.len(),
            MAX_ROLLED_BACK_ATTEMPT_TOMBSTONES,
            "must never grow past the cap regardless of how many attempts never get consumed"
        );
        assert!(
            !tombstones.contains(&0) && !tombstones.contains(&1) && !tombstones.contains(&2),
            "the oldest (first-recorded) tombstones must be the ones evicted, got: {tombstones:?}"
        );
        assert!(
            tombstones.contains(&(MAX_ROLLED_BACK_ATTEMPT_TOMBSTONES as u64 + 2)),
            "the most recently recorded tombstone must survive"
        );
    }
}

/// FQ-F9: `autonomous.enabled = false` must not fully silence the
/// `start_autonomous_component` slot-tick loop anymore. The spectral/decode
/// housekeeping (spectral snapshot, `feed_decoded_messages`,
/// `TxPlacementUpdate`) now runs unconditionally so the TX-placement
/// instrument stays live for manual-only operators, while `op.decide()` and
/// everything downstream of it (TX/action dispatch) stays gated on
/// `auto_config_enabled` — no `Transmit`/`OperatorAction`-shaped output may
/// ever escape to the message bus when disabled.
///
/// These are full coordinator-level integration tests (real
/// `ApplicationCoordinator`, real `MessageBus`, real spawned slot-tick task)
/// rather than unit tests of a pure helper, because the invariant under test
/// — "nothing escapes the message bus" — lives in the wiring of
/// `start_autonomous_component` itself, not in a function that can be
/// extracted and called directly. They wait for one real FT8 slot boundary
/// (bounded by ~15s, the same cadence the production loop aligns to) rather
/// than paused/mocked tokio time, because the loop busy-polls a
/// `tokio::select!` arm every scheduler pass (a decode-drain sub-future that
/// resolves almost immediately whenever the channel is empty) which would
/// starve `tokio::time::advance()`'s requirement that the runtime go idle
/// before jumping the clock.
#[cfg(test)]
mod fq_f9_placement_feed_when_disabled_tests {
    use super::super::ApplicationCoordinator;
    use crate::message_bus::{ComponentId, ComponentMessage, MessageType};
    use pancetta_config::Config;
    use pancetta_core::slot::SlotParity;
    use pancetta_ft8::{DecodedMessage, Ft8Message};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    /// A synthetic decoded CQ — enough to exercise
    /// `feed_decoded_messages`'s decode-history accumulation. Content is
    /// irrelevant to both tests below: the disabled test never reaches
    /// `decide()` regardless of what's fed, and the enabled control test
    /// asserts on the ALWAYS-appended `StatusUpdate` action, not on any
    /// CQ-triggered behavior.
    fn cq_decoded_message() -> DecodedMessage {
        let mut msg = DecodedMessage::new(
            Ft8Message {
                from_callsign: Some("W1AW".to_string()),
                ..Ft8Message::default()
            },
            -10.0,  // snr_db
            0.9,    // confidence
            1500.0, // frequency_offset
            0.05,   // time_offset
        );
        msg.text = "CQ W1AW FN31".to_string();
        msg.slot_parity = Some(SlotParity::Even);
        msg
    }

    async fn build_coordinator(autonomous_enabled: bool, headless: bool) -> ApplicationCoordinator {
        let mut config = Config::default();
        config.autonomous.enabled = autonomous_enabled;
        let shutdown = Arc::new(AtomicBool::new(false));
        ApplicationCoordinator::new(
            config,
            None,
            true, // no_audio
            headless,
            false, // metrics
            9090,
            None, // no WAV
            None, // no replay
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(),                                             // no config warnings
            std::env::temp_dir().join("pancetta-test-config.toml"), // test-only config path
            true,                                                   // test-only: assume TOML
        )
        .await
        .expect("coordinator creation should succeed")
    }

    /// Waits until just past the next FT8 slot boundary — the SAME
    /// `next_slot_start`/`duration_until` computation
    /// `start_autonomous_component`'s spawned task uses to align its
    /// `slot_interval`, plus a generous buffer. By the time this returns,
    /// that task's first `slot_interval.tick()` has fired. Real wall-clock
    /// (bounded to one FT8 slot, ~15s worst case) — see the module doc
    /// comment for why paused tokio time isn't used here.
    async fn wait_for_next_slot_tick() {
        let now = chrono::Utc::now();
        let next_slot = pancetta_core::slot::next_slot_start(now, chrono::Duration::zero());
        let wait =
            pancetta_core::slot::duration_until(next_slot, now) + Duration::from_millis(2500);
        tokio::time::sleep(wait).await;
    }

    fn drain(rx: &crossbeam_channel::Receiver<ComponentMessage>) -> Vec<ComponentMessage> {
        let mut out = Vec::new();
        while let Ok(m) = rx.try_recv() {
            out.push(m);
        }
        out
    }

    #[tokio::test]
    async fn disabled_feed_emits_placement_update_but_never_dispatches() {
        let mut coordinator = build_coordinator(false, true).await;

        // Subscribe to every destination the (gated) dispatch block could
        // possibly write to, PLUS Tui (the housekeeping destination), all
        // BEFORE starting the component — `create_channel` errors if called
        // twice for the same `ComponentId`, and `start_autonomous_component`
        // only ever creates its OWN (`Autonomous`) channel, so this is safe.
        let (_tui_tx, tui_rx) = coordinator
            .message_bus
            .create_channel(ComponentId::Tui)
            .await
            .expect("Tui channel should not already exist");
        let (_ft8_tx, ft8_rx) = coordinator
            .message_bus
            .create_channel(ComponentId::Ft8Transmitter)
            .await
            .expect("Ft8Transmitter channel should not already exist");
        let (_qso_tx, qso_rx) = coordinator
            .message_bus
            .create_channel(ComponentId::Qso)
            .await
            .expect("Qso channel should not already exist");

        coordinator
            .start_autonomous_component()
            .await
            .expect("start_autonomous_component must succeed even when disabled");

        // Feed exactly what the ENABLED path's own housekeeping consumes:
        // a waterfall row (drives `update_spectral`, required for
        // `placement_snapshot()` to return `Some`) and a decoded CQ (drives
        // `feed_decoded_messages`).
        coordinator
            .waterfall_to_auto_tx
            .as_ref()
            .expect("waterfall sender wired at construction time")
            .send(vec![vec![0.001f32; 100]])
            .expect("waterfall channel should accept a row");

        coordinator
            .message_bus
            .send_message(ComponentMessage::new(
                ComponentId::Ft8Decoder,
                ComponentId::Autonomous,
                MessageType::DecodedMessage(cq_decoded_message()),
                Instant::now(),
            ))
            .await
            .expect(
                "send to Autonomous should succeed — start_autonomous_component \
                 already created that channel above",
            );

        wait_for_next_slot_tick().await;

        let tui_messages = drain(&tui_rx);
        assert!(
            tui_messages
                .iter()
                .any(|m| matches!(m.message_type, MessageType::TxPlacementUpdate { .. })),
            "the TX-placement feed (spectral snapshot + decode history) must \
             keep running and emit TxPlacementUpdate even when \
             autonomous.enabled = false — this is the FQ-F9 fix"
        );
        assert!(
            !tui_messages
                .iter()
                .any(|m| matches!(m.message_type, MessageType::AutonomousStatus(_))),
            "op.decide() must NEVER run when autonomous.enabled = false — an \
             AutonomousStatus message can only be produced by decide()'s \
             unconditional status_action() append"
        );
        assert!(
            drain(&ft8_rx).is_empty(),
            "no TransmitRequest/MultiTransmitRequest may ever escape to \
             Ft8Transmitter when autonomous.enabled = false"
        );
        assert!(
            drain(&qso_rx).is_empty(),
            "no StartAutonomousQso may ever escape to Qso when \
             autonomous.enabled = false"
        );

        coordinator.shutdown_signal.store(true, Ordering::Release);
    }

    /// Control test: proves the harness (and the now-shared slot-tick loop)
    /// DOES surface decision-engine output once the gate is open — i.e. the
    /// silence asserted above is because the gate actually blocks
    /// `decide()`, not because the test fixture is inert. `decide_at`
    /// unconditionally appends a `StatusUpdate` action every cycle
    /// regardless of what else happens that tick (see
    /// `pancetta_qso::AutonomousOperator::decide_at`), so its presence is a
    /// reliable, non-flaky witness that `op.decide()` ran — independent of
    /// whether any CQ/pounce/collision decision happened to fire.
    #[tokio::test]
    async fn enabled_control_dispatches_autonomous_status_every_tick() {
        let mut coordinator = build_coordinator(true, true).await;

        let (_tui_tx, tui_rx) = coordinator
            .message_bus
            .create_channel(ComponentId::Tui)
            .await
            .expect("Tui channel should not already exist");

        coordinator
            .start_autonomous_component()
            .await
            .expect("start_autonomous_component must succeed when enabled");

        coordinator
            .waterfall_to_auto_tx
            .as_ref()
            .expect("waterfall sender wired at construction time")
            .send(vec![vec![0.001f32; 100]])
            .expect("waterfall channel should accept a row");

        wait_for_next_slot_tick().await;

        let tui_messages = drain(&tui_rx);
        assert!(
            tui_messages
                .iter()
                .any(|m| matches!(m.message_type, MessageType::TxPlacementUpdate { .. })),
            "control: TxPlacementUpdate should still appear when enabled"
        );
        assert!(
            tui_messages
                .iter()
                .any(|m| matches!(m.message_type, MessageType::AutonomousStatus(_))),
            "control failed: with autonomous.enabled = true, decide() should \
             run every tick and emit AutonomousStatus — if this fails, the \
             disabled test above proves nothing"
        );

        coordinator.shutdown_signal.store(true, Ordering::Release);
    }

    /// 2026-07-29 operator report: launching pancetta with an interactive
    /// TUI and `autonomous.enabled = true` in config immediately started
    /// dispatching autonomous TX — no `a` keypress needed. Not desirable:
    /// an interactive launch must always require the operator to
    /// explicitly arm it, matching the same safety-driver posture as
    /// Shift+Q. Auto-arming straight from config is reserved for
    /// `--headless` (see the control test below), where there is no TUI
    /// to press `a` in.
    #[tokio::test]
    async fn interactive_launch_never_auto_arms_even_when_config_enabled() {
        let mut coordinator = build_coordinator(true, false).await;

        coordinator
            .start_autonomous_component()
            .await
            .expect("start_autonomous_component must succeed for an interactive launch");

        assert!(
            !coordinator
                .autonomous_enabled_runtime
                .load(Ordering::Acquire),
            "an interactive (non-headless) launch must start with the runtime \
             gate closed even when autonomous.enabled = true in config — the \
             operator must press 'a' to arm autonomous TX"
        );

        coordinator.shutdown_signal.store(true, Ordering::Release);
    }

    /// Control test: proves the fix above is conditioned on `--headless`,
    /// not a blanket "never auto-arm" that would break the documented
    /// supervised-headless workflow (`docs/RUNBOOK.md`: "for a supervised
    /// headless station, config is the only switch" — there is no TUI to
    /// press `a` in, so config-driven auto-arm must still work there).
    #[tokio::test]
    async fn headless_launch_still_auto_arms_from_config_per_runbook() {
        let mut coordinator = build_coordinator(true, true).await;

        coordinator
            .start_autonomous_component()
            .await
            .expect("start_autonomous_component must succeed for a headless launch");

        assert!(
            coordinator
                .autonomous_enabled_runtime
                .load(Ordering::Acquire),
            "a --headless launch must still auto-arm straight from \
             autonomous.enabled = true — RUNBOOK.md documents config as the \
             only switch for a supervised headless station, and there is no \
             TUI to press 'a' in as an alternative"
        );

        coordinator.shutdown_signal.store(true, Ordering::Release);
    }
}
