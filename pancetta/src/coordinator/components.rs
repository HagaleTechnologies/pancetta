use anyhow::Result;
use pancetta_ft8::{Ft8Encoder, Ft8Modulator};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::time::{interval, sleep};
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};

impl super::ApplicationCoordinator {
    pub(crate) async fn start_autonomous_component(&mut self) -> Result<()> {
        let span = span!(Level::INFO, "start_autonomous");
        let _enter = span.enter();

        let config = self.config.read().await;
        let auto_config_enabled = config.autonomous.enabled;

        if !auto_config_enabled {
            info!("Autonomous operator disabled in configuration");
            drop(config);
            let _ = self
                .message_bus
                .create_channel(ComponentId::Autonomous)
                .await?;
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
                            let mut tx_items: Vec<crate::message_bus::TransmitRequestItem> = Vec::new();

                            for action in actions {
                                match action {
                                    pancetta_qso::OperatorAction::Transmit {
                                        ref message_text,
                                        frequency_offset,
                                        ref qso_id,
                                    } => {
                                        if qso_id.is_none() {
                                            info!(
                                                "Autonomous: opening slot at {:.0} Hz: {}",
                                                frequency_offset, message_text
                                            );
                                        }
                                        tx_items.push(crate::message_bus::TransmitRequestItem {
                                            message_text: message_text.clone(),
                                            frequency_offset,
                                            qso_id: qso_id.clone(),
                                        });
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

                            // Bundle collected TX items into a single message.
                            if tx_items.len() == 1 {
                                let item = tx_items.remove(0);
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::TransmitRequest {
                                        message_text: item.message_text,
                                        frequency_offset: item.frequency_offset,
                                        qso_id: item.qso_id,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send TransmitRequest: {}", e);
                                }
                            } else if tx_items.len() > 1 {
                                info!("Bundling {} TX items into MultiTransmitRequest", tx_items.len());
                                let msg = ComponentMessage::new(
                                    ComponentId::Autonomous,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::MultiTransmitRequest { items: tx_items },
                                    Instant::now(),
                                );
                                if let Err(e) = message_bus.send_message(msg).await {
                                    warn!("Failed to send MultiTransmitRequest: {}", e);
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

    /// Start DX cluster component for real-time spot monitoring
    pub(crate) async fn start_dx_cluster_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.dx_cluster.enabled {
            info!("DX cluster disabled in configuration");
            drop(config);
            // Still create channel so message bus doesn't complain
            let _ = self
                .message_bus
                .create_channel(ComponentId::DxCluster)
                .await?;
            return Ok(());
        }

        let cluster_hostname = config
            .network
            .dx_cluster
            .servers
            .first()
            .map(|s| s.hostname.clone())
            .unwrap_or_else(|| "dxc.nc7j.com".to_string());
        let cluster_port = config
            .network
            .dx_cluster
            .servers
            .first()
            .map(|s| s.port)
            .unwrap_or(23);
        let our_callsign = config.station.callsign.clone();
        drop(config);

        info!(
            "Starting DX cluster component ({}:{})",
            cluster_hostname, cluster_port
        );

        let (_dx_tx, _dx_rx) = self
            .message_bus
            .create_channel(ComponentId::DxCluster)
            .await?;
        let message_bus = self.message_bus.clone();

        let dx_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::cluster::{ClusterConfig, DxClusterClient};

                let mut client = DxClusterClient::with_config(ClusterConfig {
                    hostname: cluster_hostname.clone(),
                    port: cluster_port,
                    callsign: our_callsign.clone(),
                    timeout_seconds: 30,
                    reconnect_delay_seconds: 30,
                    auto_reconnect: true,
                    filter_settings: Default::default(),
                    use_websocket: false,
                    websocket_url: None,
                });

                match client.connect().await {
                    Ok(_) => {
                        info!("Connected to DX cluster");

                        // Login with our callsign
                        if let Err(e) = client.login().await {
                            warn!("DX cluster login failed: {}. Continuing without.", e);
                        }

                        // Monitor spots and forward to TUI
                        while !shutdown.load(Ordering::Acquire) {
                            match tokio::time::timeout(
                                Duration::from_secs(5),
                                client.receive_spot(),
                            )
                            .await
                            {
                                Ok(Some(spot)) => {
                                    debug!(
                                        "DX spot: {} on {} Hz by {}",
                                        spot.callsign, spot.frequency, spot.spotter
                                    );

                                    let msg = ComponentMessage::new(
                                        ComponentId::DxCluster,
                                        ComponentId::Tui,
                                        MessageType::DxMessage(
                                            crate::message_bus::DxMessage::Spot {
                                                callsign: spot.callsign,
                                                frequency: spot.frequency,
                                                spotter: spot.spotter,
                                                comment: spot.comment.unwrap_or_default(),
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = message_bus.send_message(msg).await {
                                        debug!("Failed to forward DX spot: {}", e);
                                    }
                                }
                                Ok(None) => {
                                    // No spot available, yield
                                    tokio::task::yield_now().await;
                                }
                                Err(_) => {
                                    // Timeout -- normal, just loop
                                }
                            }
                        }
                    }
                    Err(e) => {
                        warn!("Failed to connect to DX cluster: {}. Feature disabled.", e);
                    }
                }

                info!("DX cluster component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::DxCluster, dx_handle));
        info!("DX cluster component started");
        Ok(())
    }

    /// Start PSKReporter upload component
    ///
    /// Receives decoded FT8 messages, batches them, and uploads to PSKReporter
    /// at the configured interval (default: 5 minutes).
    pub(crate) async fn start_pskreporter_component(&mut self) -> Result<()> {
        let config = self.config.read().await;
        if !config.network.psk_reporter.enabled {
            info!("PSKReporter upload disabled in configuration");
            drop(config);
            let _ = self
                .message_bus
                .create_channel(ComponentId::PskReporter)
                .await?;
            return Ok(());
        }

        let our_callsign = config.station.callsign.clone();
        let our_grid = config.station.grid_square.clone();
        let upload_interval = config.network.psk_reporter.upload_interval_seconds;
        let antenna = config
            .network
            .psk_reporter
            .reporter_info
            .antenna_info
            .clone()
            .unwrap_or_default();
        let software = format!(
            "{}/{}",
            config.network.psk_reporter.reporter_info.software_name,
            config.network.psk_reporter.reporter_info.software_version
        );
        drop(config);

        info!(
            "Starting PSKReporter upload component (interval: {}s)",
            upload_interval
        );

        let (_psk_tx, psk_rx) = self
            .message_bus
            .create_channel(ComponentId::PskReporter)
            .await?;

        let psk_operating_freq = self.operating_frequency_hz.clone();
        let psk_handle = {
            let shutdown = self.shutdown_signal.clone();

            tokio::spawn(async move {
                use pancetta_dx::pskreporter::{
                    PskReporterUploadConfig, PskReporterUploader, ReceptionReport,
                };

                let upload_config = PskReporterUploadConfig {
                    reporter_callsign: our_callsign,
                    reporter_grid: our_grid,
                    antenna,
                    software,
                    upload_interval_secs: upload_interval,
                    ..Default::default()
                };

                let mut uploader = PskReporterUploader::new(upload_config);
                let mut upload_timer = interval(Duration::from_secs(upload_interval));

                while !shutdown.load(Ordering::Acquire) {
                    // Drain incoming decoded messages
                    loop {
                        match psk_rx.try_recv() {
                            Ok(message) => {
                                if let MessageType::DecodedMessage(ref decoded_msg) =
                                    message.message_type
                                {
                                    let timestamp = std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_secs()
                                        as i64;

                                    if let Some(ref callsign) = decoded_msg.message.from_callsign {
                                        let dial_freq = psk_operating_freq.load(Ordering::Relaxed);
                                        uploader.add_report(ReceptionReport {
                                            tx_callsign: callsign.clone(),
                                            frequency: dial_freq
                                                + decoded_msg.frequency_offset as u64,
                                            snr: Some(decoded_msg.snr_db as i32),
                                            mode: "FT8".to_string(),
                                            tx_grid: decoded_msg.message.grid_square.clone(),
                                            timestamp,
                                        });
                                    }
                                }
                            }
                            Err(crossbeam_channel::TryRecvError::Empty) => break,
                            Err(crossbeam_channel::TryRecvError::Disconnected) => {
                                info!("PSKReporter channel disconnected");
                                return Ok(());
                            }
                        }
                    }

                    // Check if it's time to upload
                    tokio::select! {
                        _ = upload_timer.tick() => {
                            if uploader.pending_count() > 0 {
                                match uploader.flush().await {
                                    Ok(count) => {
                                        info!("PSKReporter: uploaded {} spots", count);
                                    }
                                    Err(e) => {
                                        warn!("PSKReporter upload failed: {}", e);
                                    }
                                }
                            }
                        }
                        _ = sleep(Duration::from_millis(100)) => {
                            // Short sleep to avoid busy-looping
                        }
                    }
                }

                // Flush remaining on shutdown
                if uploader.pending_count() > 0 {
                    if let Err(e) = uploader.flush().await {
                        warn!("PSKReporter final flush failed: {}", e);
                    }
                }

                info!("PSKReporter component stopped");
                Ok(())
            })
        };

        self.named_task_handles
            .push((ComponentId::PskReporter, psk_handle));
        info!("PSKReporter component started");
        Ok(())
    }
}
