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
        let autonomous_runtime_gate = self.autonomous_enabled_runtime.clone();
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
