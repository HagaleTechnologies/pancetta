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
use tracing::{error, info, span, warn, Level};

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
        qso_starts.push(AutonomousQsoStart {
            callsign,
            frequency,
            parity,
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

impl super::ApplicationCoordinator {
    pub(crate) async fn start_autonomous_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_autonomous");
        let _enter = span.enter();

        let config = self.config.read().await;
        let auto_config_enabled = config.autonomous.enabled;

        // hb-161: seed the runtime gate from the configured value. The
        // TUI's OperatorEmergencyStop handler flips this to `false` on
        // Shift+Q; the autonomous loop checks it before submitting any
        // TX. Doing the seed here means: if the operator launched with
        // autonomous=false in config, the gate is already `false` and
        // any Q-press is a no-op (idempotent — that's the desired
        // safety-driver property).
        self.autonomous_enabled_runtime
            .store(auto_config_enabled, Ordering::Release);

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
            .create_channel(ComponentId::Autonomous)
            .await?;
        let message_bus = self.message_bus.clone();
        let display_feed_enabled = self.display_feed_enabled.clone();

        let cqdx_bridge_for_auto = self.cqdx_bridge.clone();
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
        let auto_handle = {
            let shutdown = self.shutdown_signal.clone();
            let operator = operator.clone();
            let evaluator = evaluator.clone();

            tokio::spawn(async move {
                info!("Autonomous operator started");

                let mut slot_messages: Vec<pancetta_qso::DecodedMessageInfo> = Vec::new();
                // Task 15: coordinator-local edge-trigger baseline for the
                // persistent tx.placement DiagnosticEvent, scoped to THIS
                // task's own loop. Deliberately separate from the TUI-side
                // `App::park_coverage_last` (Task 14) — different process,
                // different update cadence, different purpose (retained
                // diagnostic history vs. a transient status line) — the two
                // are never unified.
                let mut last_coverage: Option<u8> = None;
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
                            // Report decoded spots to cqdx.io
                            if let Some(ref bridge) = cqdx_bridge_for_auto {
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
                            drop(op);

                            // Collect Transmit actions, then bundle into a
                            // single MultiTransmitRequest (or single TransmitRequest).
                            let mut tx_items: Vec<(crate::message_bus::TransmitRequestItem, Option<pancetta_core::slot::SlotParity>)> = Vec::new();

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
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Qso,
                                    MessageType::QsoMessage(
                                        crate::message_bus::QsoMessage::StartAutonomousQso {
                                            callsign: start.callsign,
                                            frequency: start.frequency,
                                            parity: start.parity,
                                        },
                                    ),
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send StartAutonomousQso: {}", e);
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
                                        if let MessageType::DecodedMessage(decoded_msg) = message.message_type {
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
        let plan = plan_slot_transmissions(items, false, TxPolicy::Full, false, &[], true);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::RespondOnly, false, &[], true);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::Disabled, false, &[], true);
        assert!(plan.qso_starts.is_empty());
        assert_eq!(plan.policy_dropped, 1);
        assert_eq!(plan.tx_items.len(), 1);
    }

    // --- Full policy: opening → QSO-start split ---------------------------

    #[test]
    fn full_policy_pounce_becomes_qso_start_on_dx_freq() {
        let decodes = [decode("VB7F", 1500.0, SlotParity::Odd)];
        let items = vec![opening("VB7F K5ARH EM10", 600.0)];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &decodes, true);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &[], true);
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
    fn full_policy_in_progress_item_stays_on_raw_tx_path() {
        let items = vec![in_progress("VB7F K5ARH R-09", "q1")];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &[], true);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &decodes, true);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, true, &[], true);
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
        let plan = plan_slot_transmissions(items, false, TxPolicy::Full, false, &[], true);
        assert!(plan.qso_starts.is_empty());
        assert!(plan.tx_items.is_empty());
        assert_eq!(plan.runtime_gate_dropped, 1);
        assert_eq!(
            plan.policy_dropped, 0,
            "runtime gate already cleared the list"
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

    async fn build_coordinator(autonomous_enabled: bool) -> ApplicationCoordinator {
        let mut config = Config::default();
        config.autonomous.enabled = autonomous_enabled;
        let shutdown = Arc::new(AtomicBool::new(false));
        ApplicationCoordinator::new(
            config,
            None,
            true,  // no_audio
            true,  // headless
            false, // metrics
            9090,
            None, // no WAV
            None, // no test-tx
            1500.0,
            shutdown,
            Vec::new(), // no config warnings
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
        let mut coordinator = build_coordinator(false).await;

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
        let mut coordinator = build_coordinator(true).await;

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
}
