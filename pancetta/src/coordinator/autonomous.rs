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
) -> SlotPlan {
    // (1) Shift+Q runtime gate: closed → drop everything this cycle.
    let mut runtime_gate_dropped = 0;
    if !runtime_gate_open && !tx_items.is_empty() {
        runtime_gate_dropped = tx_items.len();
        tx_items.clear();
    }

    // (2) TX policy: suppress autonomous *initiations* (qso_id == None) unless
    // the policy allows initiation. QSO-in-progress items (qso_id == Some) flow.
    let mut policy_dropped = 0;
    if !policy.allows_initiation() {
        let before = tx_items.len();
        tx_items.retain(|(item, _)| item.qso_id.is_some());
        policy_dropped = before - tx_items.len();
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

        if !auto_config_enabled {
            info!("Autonomous operator disabled in configuration");
            drop(config);
            // The decoder fans every decoded message out to Autonomous via the
            // message bus unconditionally. If we created the channel without a
            // reader, it would fill within a few cycles and emit a continuous
            // "Channel full" warning flood (10k+ warnings/session observed in
            // the 2026-05-30 live capture). Spawn a noop drain task so the
            // channel stays open but messages are silently discarded.
            let (_drain_tx, drain_rx) = self
                .message_bus
                .create_channel(ComponentId::Autonomous)
                .await?;
            let shutdown = self.shutdown_signal.clone();
            let drain_handle = tokio::spawn(async move {
                while !shutdown.load(Ordering::Acquire) {
                    loop {
                        match drain_rx.try_recv() {
                            Ok(_) => {}
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                debug!("Autonomous drain channel disconnected");
                                return Ok(());
                            }
                        }
                    }
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
                Ok(())
            });
            self.named_task_handles
                .push((ComponentId::Autonomous, drain_handle));
            return Ok(());
        }

        info!("Starting autonomous operator component");

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
        };

        let dry_run = config.autonomous.dry_run;
        if dry_run {
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
        };
        drop(config);

        let cached_lookup = self.cached_lookup.clone();

        let spot_reporter_callsign = our_callsign.clone();
        let spot_reporter_grid = our_grid.clone();
        let operator = std::sync::Arc::new(tokio::sync::Mutex::new(
            pancetta_qso::AutonomousOperator::new(qso_auto_config, our_callsign, our_grid),
        ));

        // Phase-5 hardening #1: install the same callsign-continuity FP
        // filter the decoder uses, so the TX decision path can reject
        // CQs from callsigns absent from the trust set (defense in
        // depth — the decode-side filter still runs in
        // coordinator/ft8.rs).
        if let Some(ref filter) = self.fp_filter {
            let filter = filter.clone();
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

        let (waterfall_to_auto_tx, waterfall_to_auto_rx) =
            crossbeam_channel::bounded::<Vec<Vec<f32>>>(2);
        self.waterfall_to_auto_tx = Some(waterfall_to_auto_tx);

        let evaluator: std::sync::Arc<dyn pancetta_qso::DxEvaluator> = std::sync::Arc::new(
            pancetta_qso::PriorityScorer::new(priority_weights, Box::new((*cached_lookup).clone())),
        );

        let (_auto_tx, auto_rx) = self
            .message_bus
            .create_channel(ComponentId::Autonomous)
            .await?;
        let message_bus = self.message_bus.clone();

        let cqdx_bridge_for_auto = self.cqdx_bridge.clone();
        let operating_frequency_hz = self.operating_frequency_hz.clone();
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
        // Phase 5: the active-QSO set the QSO component maintains. Its length is
        // fed to the operator each slot as `active_qso_count` so the decision
        // engine's `max_concurrent_qsos` gate sees QSOs the engine itself is
        // now driving (autonomous Auto QSOs land here once created), and so we
        // don't open a second pounce while one is already in progress.
        let active_tx_qsos = self.active_tx_qsos.clone();
        let auto_handle = {
            let shutdown = self.shutdown_signal.clone();
            let operator = operator.clone();
            let evaluator = evaluator.clone();

            tokio::spawn(async move {
                info!("Autonomous operator started");

                let mut slot_messages: Vec<pancetta_qso::DecodedMessageInfo> = Vec::new();
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
                                        freq_min_hz: 200.0,
                                        freq_max_hz: 3000.0,
                                    });
                                }
                            }

                            if let Some(ref bridge) = cqdx_bridge_for_auto {
                                let spot_freqs = bridge.spot_frequencies().await;
                                op.update_live_spots(&spot_freqs);
                            }

                            op.feed_decoded_messages(&slot_messages, evaluator.as_ref());
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
                            let listen_messages = slot_messages.clone();
                            slot_messages.clear();
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
                                            match ca {
                                                pancetta_qso::OperatorAction::FrequencyShift { new_offset_hz } => {
                                                    info!("Collision listen: TX offset shifted to {:.0} Hz", new_offset_hz);
                                                }
                                                _ => {}
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
                            let plan = plan_slot_transmissions(
                                tx_items,
                                runtime_gate_open,
                                policy,
                                dry_run,
                                &listen_messages,
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
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        warn!("Failed to send TransmitRequest: {}", e);
                                    }
                                }
                            } else if tx_items.len() > 1 {
                                let bundle_parity = tx_items[0].1;
                                for (idx, (_, p)) in tx_items.iter().enumerate().skip(1) {
                                    if *p != bundle_parity {
                                        warn!(
                                            "Multi-TX item {} has tx_parity {:?}, bundle is {:?}; \
                                             using bundle parity",
                                            idx, p, bundle_parity
                                        );
                                    }
                                }
                                let items: Vec<_> = tx_items.into_iter().map(|(it, _)| it).collect();
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
                                        },
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        warn!("Failed to send MultiTransmitRequest: {}", e);
                                    }
                                }
                            }
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
        let plan = plan_slot_transmissions(items, false, TxPolicy::Full, false, &[]);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::RespondOnly, false, &[]);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::Disabled, false, &[]);
        assert!(plan.qso_starts.is_empty());
        assert_eq!(plan.policy_dropped, 1);
        assert_eq!(plan.tx_items.len(), 1);
    }

    // --- Full policy: opening → QSO-start split ---------------------------

    #[test]
    fn full_policy_pounce_becomes_qso_start_on_dx_freq() {
        let decodes = [decode("VB7F", 1500.0, SlotParity::Odd)];
        let items = vec![opening("VB7F K5ARH EM10", 600.0)];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &decodes);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &[]);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &[]);
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
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, false, &decodes);
        assert_eq!(plan.qso_starts.len(), 1, "the opening → start");
        assert_eq!(plan.tx_items.len(), 1, "the in-progress item → raw TX");
        assert_eq!(plan.tx_items[0].0.qso_id.as_deref(), Some("q7"));
    }

    // --- dry_run ----------------------------------------------------------

    #[test]
    fn dry_run_records_openings_without_creating_qsos() {
        let items = vec![
            opening("VB7F K5ARH EM10", 600.0),
            in_progress("W1AW K5ARH R-12", "q7"),
        ];
        let plan = plan_slot_transmissions(items, true, TxPolicy::Full, true, &[]);
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
        let plan = plan_slot_transmissions(items, false, TxPolicy::Full, false, &[]);
        assert!(plan.qso_starts.is_empty());
        assert!(plan.tx_items.is_empty());
        assert_eq!(plan.runtime_gate_dropped, 1);
        assert_eq!(
            plan.policy_dropped, 0,
            "runtime gate already cleared the list"
        );
    }
}
