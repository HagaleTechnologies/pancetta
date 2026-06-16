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

                            // hb-161: gate TX dispatch on the runtime
                            // operator-override flag. If the operator
                            // pressed Shift+Q, drop any TX items the
                            // decision engine produced this cycle and
                            // log once at WARN so the disengagement is
                            // visible in journals. Listen/Status/Band
                            // actions are still forwarded — only
                            // outgoing TX is suppressed. The autonomous
                            // operator retains its internal state so
                            // re-enabling later picks up cleanly.
                            if !autonomous_runtime_gate.load(Ordering::Acquire)
                                && !tx_items.is_empty()
                            {
                                warn!(
                                    target: "operator.override",
                                    "Autonomous runtime gate is OFF; dropping {} TX items \
                                     produced this cycle (operator pressed Shift+Q)",
                                    tx_items.len()
                                );
                                tx_items.clear();
                            }

                            // Tri-state TX policy: when the policy disallows
                            // initiation (RespondOnly or Disabled), drop the
                            // autonomous *initiation* items — calling CQ
                            // ourselves and hunting/pouncing on a CQer, both
                            // identified by `qso_id == None`. Items belonging
                            // to a QSO already in progress (`qso_id == Some`)
                            // are kept so RespondOnly continues those exchanges;
                            // Disabled additionally hard-mutes them at the TX
                            // worker. The decision engine's internal state is
                            // untouched, so returning to Full resumes cleanly.
                            {
                                let policy = pancetta_core::TxPolicy::from_u8(
                                    tx_policy.load(Ordering::Acquire),
                                );
                                if !policy.allows_initiation() {
                                    let before = tx_items.len();
                                    tx_items.retain(|(item, _)| item.qso_id.is_some());
                                    let dropped = before - tx_items.len();
                                    if dropped > 0 {
                                        info!(
                                            target: "tx.policy",
                                            "TX policy {}: suppressing {} autonomous initiation \
                                             item(s) this cycle (QSO-in-progress items kept)",
                                            policy.label(),
                                            dropped
                                        );
                                    }
                                }
                            }

                            // Phase 5: route surviving autonomous *openings*
                            // (qso_id == None) through the QSO component so the
                            // QsoManager owns the exchange and auto-sequences it
                            // to completion. This runs AFTER the runtime gate and
                            // TX-policy initiation suppression above, so a
                            // suppressed cycle never creates a QSO. The opening is
                            // sent via StartAutonomousQso INSTEAD OF a raw
                            // TransmitRequest (the QsoManager emits the opening
                            // itself, on the DX's frequency); we drop it from
                            // tx_items to avoid a double-send. QSO-in-progress
                            // items (qso_id == Some) stay on the raw TX path.
                            {
                                let mut remaining: Vec<_> = Vec::with_capacity(tx_items.len());
                                for (item, tx_parity) in tx_items.drain(..) {
                                    if item.qso_id.is_some() {
                                        remaining.push((item, tx_parity));
                                        continue;
                                    }
                                    if dry_run {
                                        info!(
                                            target: "autonomous.dry_run",
                                            "DRY RUN: would have opened autonomous QSO from '{}'",
                                            item.message_text
                                        );
                                        continue;
                                    }
                                    let (callsign, frequency, parity) =
                                        classify_autonomous_opening(
                                            &item.message_text,
                                            item.frequency_offset,
                                            tx_parity,
                                            &listen_messages,
                                        );
                                    let msg = ComponentMessage::new(
                                        ComponentId::Autonomous,
                                        ComponentId::Qso,
                                        MessageType::QsoMessage(
                                            crate::message_bus::QsoMessage::StartAutonomousQso {
                                                callsign,
                                                frequency,
                                                parity,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        warn!("Failed to send StartAutonomousQso: {}", e);
                                    }
                                }
                                tx_items = remaining;
                            }

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
