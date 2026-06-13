//! TUI relay + command-forwarding component.
//!
//! Two cooperating tasks plus the TUI runner itself live here:
//!
//! 1. **Decoded-message + bus relay** (`tui-relay` std::thread). Reads
//!    decoded FT8 messages, control-bus messages (autonomous status,
//!    frequency response, DX spots, status updates, errors), waterfall
//!    rows, and audio-level samples; converts each into the right
//!    `TuiMessage` variant; pushes to the TUI message channel. Runs as a
//!    std::thread (not tokio) to avoid runtime starvation when the FT8
//!    decoder is busy. Once every 2 seconds it also synthesizes a
//!    `PipelineHealth` snapshot from the shared atomics.
//!
//! 2. **Command-forwarding loop** (`tokio::spawn`). Reads
//!    `TuiCommand` from the runner (Space-to-call, frequency change, PTT
//!    toggle, start/stop CQ, etc.) and translates each into the right
//!    `MessageBus` message routed at the right component. Also drives
//!    the repeating-CQ timer when the operator presses 'c'.
//!
//! 3. **TUI runner** (`tokio::task::spawn_blocking`). Owns the terminal,
//!    runs ratatui's draw loop, and exits when the user quits — at which
//!    point we trigger global shutdown so the rest of the coordinator
//!    tears down cleanly.

use anyhow::Result;
use geographiclib_rs::InverseGeodesic;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tracing::{debug, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

impl super::ApplicationCoordinator {
    /// Start TUI component with point-to-point decoded message channel
    pub(crate) async fn start_tui_pipeline(
        &mut self,
        ft8_to_tui_rx: crossbeam_channel::Receiver<pancetta_ft8::DecodedMessage>,
        tui_bus_rx: crossbeam_channel::Receiver<ComponentMessage>,
        waterfall_rx: crossbeam_channel::Receiver<Vec<Vec<f32>>>,
        audio_level_rx: crossbeam_channel::Receiver<f32>,
        health_audio_alive: Arc<std::sync::atomic::AtomicBool>,
        health_dsp_windows: Arc<std::sync::atomic::AtomicU64>,
        health_last_rms: Arc<std::sync::atomic::AtomicU32>,
        health_total_decodes: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_tui");
        let _enter = span.enter();

        info!("Starting TUI component");

        let config = self.config.clone();
        let shutdown = self.shutdown_signal.clone();

        // Create TUI message/command channels for the TuiRunner
        let (tui_msg_tx, tui_msg_rx) =
            crossbeam_channel::bounded::<pancetta_tui::tui_runner::TuiMessage>(1000);
        let (tui_cmd_tx, tui_cmd_rx) =
            crossbeam_channel::bounded::<pancetta_tui::tui_runner::TuiCommand>(1000);

        // Use the rig's current frequency if hamlib has already read it,
        // otherwise fall back to 14.074 MHz. Updated by FrequencyResponse messages.
        let current_hz = self.operating_frequency_hz.load(Ordering::Relaxed);
        let operating_freq_mhz = if current_hz > 0 {
            current_hz as f64 / 1_000_000.0
        } else {
            14.074_f64
        };
        let operating_freq = Arc::new(std::sync::atomic::AtomicU64::new(
            operating_freq_mhz.to_bits(),
        ));
        let operating_freq_relay = operating_freq.clone();

        // Set up station coordinates for distance/bearing calculation
        let station_coords = {
            let config = self.config.read().await;
            pancetta_core::gridsquare::grid_to_coordinates(&config.station.grid_square).ok()
        };
        // Our callsign — used to flag "calling me" decodes (to_callsign
        // matches us). Cached at thread spawn so we don't take the config
        // lock on every decode.
        let our_callsign_for_relay = {
            let config = self.config.read().await;
            config.station.callsign.clone()
        };

        // Batch 95: worked-before enrichment. This is the SAME
        // Arc<CachedStationLookup> the autonomous priority scorer reads
        // for its duplicate penalty — seeded from ~/.pancetta/qso.db at
        // QSO-component startup and updated in-memory by record_worked
        // on every completed QSO — so the TUI's worked-before flag can
        // never disagree with the scorer. Lookups are an in-memory
        // HashSet probe behind a parking_lot read lock; the relay
        // thread (not the render loop) pays that cost, and the TUI just
        // renders the precomputed bool.
        let relay_station_lookup = self.cached_lookup.clone();

        // Relay decoded messages from FT8 -> TUI on a dedicated thread
        // (tokio::spawn was causing starvation -- same pattern as DSP/FT8 fixes)
        let relay_shutdown = shutdown.clone();
        let tui_msg_tx_relay = tui_msg_tx.clone();
        // Runtime autonomous gate (the flag Shift+Q clears and `a`
        // re-sets). The relay reads it (never writes) so the live
        // `[AUTO]` panel shows enabled=false while the operator
        // override is active, even though the qso-crate operator's
        // internal `enabled` stays true.
        let relay_autonomous_gate = self.autonomous_enabled_runtime.clone();
        let health_audio_alive_relay = health_audio_alive.clone();
        let health_dsp_windows_relay = health_dsp_windows.clone();
        let health_last_rms_relay = health_last_rms.clone();
        let health_total_decodes_relay = health_total_decodes.clone();
        let tui_relay_jh = std::thread::Builder::new()
            .name("tui-relay".to_string())
            .spawn(move || {
            let mut ft8_disconnected = false;
            let mut last_health_send = std::time::Instant::now();
            while !relay_shutdown.load(Ordering::Acquire) {
                if !ft8_disconnected {
                    match ft8_to_tui_rx.try_recv() {
                        Ok(decoded_msg) => {
                            let call_sign = decoded_msg.message.from_callsign.clone();
                            let grid_square = decoded_msg.message.grid_square.clone();
                            // "Calling me" detection: the parser sets
                            // to_callsign = our call when someone replies
                            // to our CQ ("K5ARH F5ABC -10"). Match against
                            // the bare callsign — strip any /R or /P suffix
                            // on either side so "K5ARH/M" and "K5ARH" both
                            // count.
                            let is_directed_at_us = match decoded_msg.message.to_callsign.as_deref() {
                                Some(to) => {
                                    let to_base = to.split('/').next().unwrap_or(to);
                                    let our_base = our_callsign_for_relay
                                        .split('/')
                                        .next()
                                        .unwrap_or(&our_callsign_for_relay);
                                    !to_base.is_empty()
                                        && !our_base.is_empty()
                                        && to_base.eq_ignore_ascii_case(our_base)
                                }
                                None => false,
                            };

                            // Compute distance and bearing if both grids are available
                            let (distance, bearing) = match (&grid_square, &station_coords) {
                                (Some(remote_grid), Some((home_lat, home_lon))) => {
                                    match pancetta_core::gridsquare::grid_to_coordinates(remote_grid)
                                    {
                                        Ok((remote_lat, remote_lon)) => {
                                            let geod = geographiclib_rs::Geodesic::wgs84();
                                            let (dist_m, azi1, _azi2, _arc) = geod.inverse(
                                                *home_lat, *home_lon, remote_lat, remote_lon,
                                            );
                                            let bearing_deg =
                                                if azi1 < 0.0 { azi1 + 360.0 } else { azi1 };
                                            (Some(dist_m / 1000.0), Some(bearing_deg))
                                        }
                                        Err(_) => (None, None),
                                    }
                                }
                                _ => (None, None),
                            };

                            // Worked-before: same semantics as the scorer's
                            // duplicate penalty — band-scoped (current
                            // operating frequency), uppercase-exact match on
                            // the full callsign. We deliberately do NOT strip
                            // /P-style suffixes: record_worked stores the
                            // callsign exactly as logged, and adding
                            // stripping on the TUI side only would make the
                            // TUI flag stations the scorer still treats as
                            // new (divergence).
                            let dial_mhz =
                                f64::from_bits(operating_freq_relay.load(Ordering::Relaxed));
                            let worked_before = worked_before_for(
                                &relay_station_lookup,
                                call_sign.as_deref(),
                                dial_mhz * 1_000_000.0,
                            );

                            let tui_decoded = pancetta_tui::DecodedMessageView {
                                timestamp: chrono::Utc::now(),
                                frequency: dial_mhz,
                                mode: "FT8".to_string(),
                                snr: decoded_msg.snr_db as i32,
                                delta_time: decoded_msg.time_offset as f32,
                                delta_freq: decoded_msg.frequency_offset as f32,
                                call_sign,
                                grid_square,
                                message: decoded_msg.text.clone(),
                                distance,
                                bearing,
                                slot_parity: decoded_msg.slot_parity,
                                is_directed_at_us,
                                worked_before,
                            };

                            match tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DecodedMessage(tui_decoded),
                            ) {
                                Ok(()) => info!("TUI relay: forwarded decoded message to TUI channel"),
                                Err(e) => warn!("TUI relay: failed to send to TUI: {}", e),
                            }
                        }
                        Err(crossbeam_channel::TryRecvError::Empty) => {}
                        Err(crossbeam_channel::TryRecvError::Disconnected) => {
                            warn!("FT8 decoder channel disconnected, TUI relay continuing without decode data");
                            ft8_disconnected = true;
                        }
                    }
                }

                // Also drain control messages from the message bus
                match tui_bus_rx.try_recv() {
                    Ok(bus_msg) => {
                        match bus_msg.message_type {
                            MessageType::AutonomousStatus(ref status) => {
                                // Batch 93: forward the STRUCTURED status so the
                                // live `[AUTO]` panel renders (previously this was
                                // flattened to a transient status-bar string and
                                // `app.autonomous_status` stayed None forever).
                                let mapped = map_autonomous_status(
                                    status,
                                    relay_autonomous_gate.load(Ordering::Acquire),
                                );
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::AutonomousStatusUpdate(
                                        mapped,
                                    ),
                                );
                                // Keep the status-bar text line too (additive).
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "Autonomous".to_string(),
                                        status: status.state.clone(),
                                    },
                                );
                            }
                            MessageType::TxStatus { active } => {
                                // Batch 93: TX worker brackets every transmission
                                // (PTT-on → PTT-off, including aborts) with these.
                                // Drives the title-bar " TX " badge.
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::TxStatus { active },
                                );
                            }
                            MessageType::ActiveQsosSnapshot { ref qsos } => {
                                // Re-shape into the TUI's ActiveQsoBanner
                                // (decoupled struct so the TUI doesn't link
                                // pancetta_qso). Push as a TuiMessage; the
                                // TUI replaces its previous list with this.
                                // Batch 94: carries the QSO-detail panel
                                // fields too (last TX/RX message, SNR,
                                // reports, exchange count).
                                let banner_qsos: Vec<pancetta_tui::app::ActiveQsoBanner> =
                                    qsos.iter().map(map_qso_snapshot_item).collect();
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::ActiveQsosUpdate {
                                        qsos: banner_qsos,
                                    },
                                );
                            }
                            MessageType::RigControl(
                                crate::message_bus::RigControlMessage::FrequencyResponse {
                                    vfo,
                                    frequency,
                                },
                            ) => {
                                // Update operating frequency for decoded message enrichment
                                let freq_mhz = frequency as f64 / 1_000_000.0;
                                // Relaxed ordering is fine -- this is a best-effort display value for the TUI
                                operating_freq_relay.store(freq_mhz.to_bits(), Ordering::Relaxed);
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::FrequencyUpdate {
                                        vfo,
                                        frequency,
                                    },
                                );
                            }
                            MessageType::RigControl(
                                crate::message_bus::RigControlMessage::SignalStrengthResponse {
                                    db_over_s9,
                                },
                            ) => {
                                // Batch 95: real rig S-meter read (hamlib
                                // STRENGTH, dB relative to S9) from the
                                // polling loop — forward verbatim.
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::SignalStrengthUpdate {
                                        db_over_s9,
                                    },
                                );
                            }
                            MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
                                callsign,
                                frequency,
                                spotter,
                                ..
                            }) => {
                                // Worked-before keyed on the SPOT's frequency
                                // (cluster spots carry their own), same
                                // lookup/semantics as the decode path above.
                                let worked_before = worked_before_for(
                                    &relay_station_lookup,
                                    Some(callsign.as_str()),
                                    frequency as f64,
                                );
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::DxSpot {
                                        callsign,
                                        frequency,
                                        spotter,
                                        worked_before,
                                    },
                                );
                            }
                            MessageType::StatusUpdate(text) => {
                                // Free-form status emitted by other components (e.g. QSO
                                // component reports respond_to_cq success/failure here so
                                // Space-to-call surfaces "Calling X — TX queued" or the
                                // actual rejection reason instead of just an optimistic
                                // "Calling X..." that hides silent failures.
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: format!("{}", bus_msg.source),
                                        status: text,
                                    },
                                );
                            }
                            MessageType::Error {
                                component_id,
                                ref error_message,
                                ..
                            } => {
                                // Component-level errors (audio init failure, audio
                                // device stalls, etc.) get surfaced to the TUI's error
                                // log instead of dying silently in the log file. Without
                                // this hop the audio thread can fail to start and the
                                // user sees only an inert pipeline with no decodes.
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::Error {
                                        component: format!("{}", component_id),
                                        message: error_message.clone(),
                                    },
                                );
                            }
                            _ => {}
                        }
                    }
                    Err(_) => {}
                }

                // Relay waterfall data from FT8 decoder to TUI
                match waterfall_rx.try_recv() {
                    Ok(rows) => {
                        let _ = tui_msg_tx_relay
                            .send(pancetta_tui::tui_runner::TuiMessage::WaterfallUpdate { rows });
                    }
                    Err(_) => {}
                }

                // Relay audio level from DSP to TUI
                match audio_level_rx.try_recv() {
                    Ok(level) => {
                        let _ = tui_msg_tx_relay
                            .send(pancetta_tui::tui_runner::TuiMessage::AudioLevel { level });
                    }
                    Err(_) => {}
                }

                // Sleep to prevent busy-spinning
                std::thread::sleep(std::time::Duration::from_millis(10));

                // Send pipeline health to TUI every 2 seconds
                if last_health_send.elapsed() >= std::time::Duration::from_secs(2) {
                    let health = pancetta_tui::app::PipelineHealth {
                        audio_alive: health_audio_alive_relay.load(Ordering::Relaxed),
                        dsp_windows: health_dsp_windows_relay.load(Ordering::Relaxed),
                        last_rms: f32::from_bits(health_last_rms_relay.load(Ordering::Relaxed)),
                        ft8lib_available: pancetta_ft8::ft8lib_is_available(),
                        total_decodes: health_total_decodes_relay.load(Ordering::Relaxed),
                    };
                    let _ = tui_msg_tx_relay.send(
                        pancetta_tui::tui_runner::TuiMessage::PipelineHealth(health),
                    );
                    last_health_send = std::time::Instant::now();
                }
            }
            info!("TUI relay thread stopped");
        }).expect("Failed to spawn TUI relay thread");
        self.tui_relay_handle = Some(tui_relay_jh);

        // Task: relay TUI commands (e.g. SendMessage) to message bus as TransmitRequests
        let cmd_shutdown = self.shutdown_signal.clone();
        let cmd_message_bus = self.message_bus.clone();
        let cmd_operating_freq = operating_freq.clone();
        let cmd_operating_freq_hz = self.operating_frequency_hz.clone();
        // Read station config for CQ generation
        let cmd_station_call = {
            let cfg = self.config.read().await;
            cfg.station.callsign.clone()
        };
        let cmd_station_grid = {
            let cfg = self.config.read().await;
            cfg.station.grid_square.clone()
        };
        let cmd_ptt_state = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cmd_cq_active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cmd_abort_current_tx = self.abort_current_tx.clone();
        let cmd_autonomous_enabled = self.autonomous_enabled_runtime.clone();
        // Whether the autonomous component is running at all (config
        // gate). If it's config-disabled there is no decision loop to
        // re-enable — `a` should say so honestly instead of flipping a
        // gate nothing reads.
        let cmd_autonomous_config_enabled = {
            let cfg = self.config.read().await;
            cfg.autonomous.enabled
        };
        // Direct path back to the TUI so ToggleAutonomous can confirm
        // immediately (the structured panel update follows on the next
        // autonomous slot tick, ≤15s later).
        let cmd_tui_msg_tx = tui_msg_tx.clone();
        // Shared config — the SelectDevice handler persists the operator's
        // chosen output device into it (and into ~/.pancetta/pancetta.toml).
        let cmd_config = self.config.clone();
        // F4 toggle state: Some(t) when a tune is in flight and expected
        // to auto-stop at instant t. None when no tune is queued. The
        // coordinator owns this — TUI just emits ToggleTune events.
        let cmd_tune_until: std::sync::Arc<tokio::sync::RwLock<Option<tokio::time::Instant>>> =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));
        const TUNE_DURATION_SECS: u32 = 12;
        const TUNE_TONE_HZ: f64 = 1500.0;
        let cmd_handle = tokio::spawn(async move {
            let mut next_cq_time: Option<tokio::time::Instant> = None;
            // Operator's TX audio offset (Hz) for repeating CQ, set from the
            // waterfall cursor on StartCq. Default 1500 until the operator
            // moves the cursor (was previously hard-coded, ignoring the cursor).
            let mut cq_frequency_offset: f64 = 1500.0;

            // Push the available audio devices to the TUI once at startup so
            // the `d` device-selection picker can list them. The coordinator
            // owns the pancetta-audio host; the TUI is a passive renderer.
            {
                let current_output = {
                    let cfg = cmd_config.read().await;
                    let dev = cfg.audio.output_device.clone();
                    if dev.is_empty() {
                        None
                    } else {
                        Some(dev)
                    }
                };
                let input = pancetta_audio::device::list_input_devices();
                let output = pancetta_audio::device::list_output_devices();
                if let Err(e) =
                    cmd_tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::DeviceListUpdate {
                        input,
                        output,
                        current_output,
                    })
                {
                    debug!("Failed to send initial device list to TUI: {}", e);
                }
            }

            while !cmd_shutdown.load(Ordering::Acquire) {
                // Send repeating CQ every 15 seconds when active
                if cmd_cq_active.load(Ordering::Relaxed) {
                    let now = tokio::time::Instant::now();
                    if next_cq_time.map_or(true, |t| now >= t) {
                        let cq_text = format!("CQ {} {}", cmd_station_call, cmd_station_grid);
                        info!("CQ repeat: '{}'", cq_text);
                        let msg = ComponentMessage::new(
                            ComponentId::Tui,
                            ComponentId::Ft8Transmitter,
                            MessageType::TransmitRequest {
                                message_text: cq_text,
                                frequency_offset: cq_frequency_offset,
                                qso_id: None,
                                tx_parity: None, // TUI CQ repeat: no DX context
                            },
                            Instant::now(),
                        );
                        if let Err(e) = cmd_message_bus.send_message(msg).await {
                            warn!("Failed to send repeating CQ: {}", e);
                        }
                        next_cq_time = Some(now + Duration::from_secs(15));
                    }
                }

                match tui_cmd_rx.try_recv() {
                    Ok(cmd) => match cmd {
                        pancetta_tui::tui_runner::TuiCommand::SendMessage {
                            text,
                            frequency_offset,
                        } => {
                            info!(
                                "TUI SendMessage: '{}' at {:.0} Hz (waterfall cursor)",
                                text, frequency_offset
                            );
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Ft8Transmitter,
                                MessageType::TransmitRequest {
                                    message_text: text,
                                    frequency_offset,
                                    qso_id: None,
                                    tx_parity: None, // TUI manual send: no DX context
                                },
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward TUI command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::CallStation {
                            callsign,
                            frequency,
                            dx_parity,
                        } => {
                            info!("TUI CallStation: {} at {} Hz", callsign, frequency);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(crate::message_bus::QsoMessage::StartQso {
                                    callsign,
                                    frequency,
                                    dx_parity,
                                }),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward CallStation command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::SetFrequency { vfo, frequency } => {
                            info!("TUI SetFrequency: VFO {} -> {} Hz", vfo, frequency);
                            let freq_mhz = frequency as f64 / 1_000_000.0;
                            cmd_operating_freq.store(freq_mhz.to_bits(), Ordering::Relaxed);
                            cmd_operating_freq_hz.store(frequency, Ordering::Relaxed);
                            // Forward to hamlib if available
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Hamlib,
                                MessageType::RigControl(
                                    crate::message_bus::RigControlMessage::SetFrequency {
                                        vfo,
                                        frequency,
                                    },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                debug!("Failed to forward SetFrequency to hamlib: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::Quit => {
                            info!("TUI requested application quit");
                            cmd_shutdown.store(true, Ordering::Release);
                            break;
                        }
                        pancetta_tui::tui_runner::TuiCommand::StartCq { frequency_offset } => {
                            info!(
                                "TUI StartCq: enabling repeating CQ at {:.0} Hz (waterfall cursor)",
                                frequency_offset
                            );
                            cq_frequency_offset = frequency_offset;
                            cmd_cq_active.store(true, Ordering::Relaxed);
                            // Send first CQ immediately by resetting the timer
                            next_cq_time = None;
                        }
                        pancetta_tui::tui_runner::TuiCommand::StopCq => {
                            info!("TUI StopCq: stopping repeating CQ");
                            cmd_cq_active.store(false, Ordering::Relaxed);
                            next_cq_time = None;
                        }
                        pancetta_tui::tui_runner::TuiCommand::OperatorEmergencyStop => {
                            // hb-161: Phase 5 emergency stop. Operator
                            // pressed Shift+Q. Halt every TX path the
                            // station can drive:
                            //   1. Abort the in-flight TX (PTT-off in
                            //      ~50ms via the existing F8 path).
                            //   2. Disable autonomous mode at runtime
                            //      (the autonomous loop reads this flag
                            //      every slot before submitting TX).
                            //   3. Stop the repeating-CQ loop.
                            //   4. Cancel any active tune tone.
                            // Logged at WARN with target=operator.override
                            // so it stands out in the journal. The
                            // operator re-enables autonomous explicitly:
                            // the TUI `a` key sends ToggleAutonomous,
                            // which re-sets this same runtime gate
                            // (Batch 93). We don't auto-restore.
                            warn!(
                                target: "operator.override",
                                "Operator emergency stop (Shift+Q): aborting TX, disabling \
                                 autonomous, stopping CQ + tune"
                            );
                            cmd_abort_current_tx.store(true, Ordering::Release);
                            cmd_autonomous_enabled.store(false, Ordering::Release);
                            cmd_cq_active.store(false, Ordering::Relaxed);
                            *cmd_tune_until.write().await = None;
                            next_cq_time = None;
                        }
                        pancetta_tui::tui_runner::TuiCommand::StopTx => {
                            // Operator F8: abort the in-flight TX without
                            // exiting. The TX worker's interruptible_sleep
                            // wakes within ~50ms, drops PttGuard (PTT-off),
                            // and continues to the next message. The flag
                            // is reset by the worker at the start of each
                            // try_recv cycle so a stale F8 doesn't kill
                            // the next legitimate TX.
                            //
                            // Also stop the repeating-CQ loop so we don't
                            // immediately re-arm a new TX in the next cycle.
                            // Clear the tune-until tracker so the F4 toggle
                            // re-arms cleanly next press.
                            info!("TUI StopTx: halting current TX (F8)");
                            cmd_abort_current_tx.store(true, Ordering::Release);
                            cmd_cq_active.store(false, Ordering::Relaxed);
                            *cmd_tune_until.write().await = None;
                            next_cq_time = None;
                        }
                        pancetta_tui::tui_runner::TuiCommand::ToggleTune => {
                            // F4 toggle. If a tune is already in flight,
                            // abort it. Otherwise queue a new TuneRequest
                            // and arm the auto-stop tracker.
                            let now = tokio::time::Instant::now();
                            let active = {
                                let guard = cmd_tune_until.read().await;
                                matches!(*guard, Some(t) if t > now)
                            };
                            if active {
                                info!("TUI ToggleTune: aborting in-flight tune (F4)");
                                cmd_abort_current_tx.store(true, Ordering::Release);
                                *cmd_tune_until.write().await = None;
                            } else {
                                info!(
                                    "TUI ToggleTune: starting {}s tone at {} Hz",
                                    TUNE_DURATION_SECS, TUNE_TONE_HZ
                                );
                                let msg = ComponentMessage::new(
                                    ComponentId::Tui,
                                    ComponentId::Ft8Transmitter,
                                    MessageType::TuneRequest {
                                        duration_secs: TUNE_DURATION_SECS,
                                        tone_offset_hz: TUNE_TONE_HZ,
                                    },
                                    Instant::now(),
                                );
                                if let Err(e) = cmd_message_bus.send_message(msg).await {
                                    warn!("Failed to send TuneRequest: {}", e);
                                } else {
                                    *cmd_tune_until.write().await =
                                        Some(now + Duration::from_secs(TUNE_DURATION_SECS as u64));
                                }
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::ToggleAutonomous => {
                            // Batch 93: operator pressed `a`. Flip the SAME
                            // runtime gate OperatorEmergencyStop clears — this
                            // is the documented Shift+Q → `a` recovery path.
                            // Re-enabling NEVER starts a TX directly: the gate
                            // is only read by the autonomous loop before
                            // dispatching TX items its decision engine (with
                            // its own slot/priority/QSO gates) produced.
                            if !cmd_autonomous_config_enabled {
                                info!(
                                    "TUI ToggleAutonomous: autonomous disabled in config; \
                                     no decision loop to toggle"
                                );
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "Autonomous".to_string(),
                                        status: "Autonomous disabled in config — restart with \
                                                 autonomous.enabled=true"
                                            .to_string(),
                                    },
                                );
                            } else {
                                let was = cmd_autonomous_enabled.load(Ordering::Acquire);
                                let now_enabled = !was;
                                cmd_autonomous_enabled.store(now_enabled, Ordering::Release);
                                if now_enabled {
                                    warn!(
                                        target: "operator.override",
                                        "Operator re-enabled autonomous TX (a key)"
                                    );
                                } else {
                                    warn!(
                                        target: "operator.override",
                                        "Operator disabled autonomous TX (a key)"
                                    );
                                }
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "Autonomous".to_string(),
                                        status: if now_enabled {
                                            "Autonomous TX re-enabled (runtime gate open)"
                                                .to_string()
                                        } else {
                                            "Autonomous TX disabled (runtime gate closed)"
                                                .to_string()
                                        },
                                    },
                                );
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::TogglePtt => {
                            let current = cmd_ptt_state.load(Ordering::Acquire);
                            let new_state = !current;
                            cmd_ptt_state.store(new_state, Ordering::Release);
                            info!("TUI TogglePtt: {} -> {}", current, new_state);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Hamlib,
                                MessageType::RigControl(
                                    crate::message_bus::RigControlMessage::SetPtt {
                                        state: new_state,
                                    },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to toggle PTT: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::SelectDevice {
                            input_device,
                            output_device,
                        } => {
                            info!(
                                "TUI SelectDevice: in={:?} out={:?}",
                                input_device, output_device
                            );
                            // Persist the operator's choice to the in-memory
                            // config and to ~/.pancetta/pancetta.toml so it
                            // survives a restart. We do NOT reopen the live
                            // cpal output stream here — that lives in
                            // pancetta-audio's stream layer.
                            // TODO(coordinator): live output-stream switch on
                            // SelectDevice (reopen the cpal output stream
                            // without a restart).
                            {
                                let mut cfg = cmd_config.write().await;
                                if let Some(ref out) = output_device {
                                    cfg.audio.output_device = out.clone();
                                }
                                if let Some(ref inp) = input_device {
                                    cfg.audio.input_device = inp.clone();
                                }
                            }
                            let config_path = dirs::home_dir()
                                .unwrap_or_else(|| std::path::PathBuf::from("."))
                                .join(".pancetta")
                                .join("pancetta.toml");
                            let persist_result = {
                                let cfg = cmd_config.read().await;
                                cfg.set_audio_devices_in_file(
                                    &config_path,
                                    input_device.as_deref(),
                                    output_device.as_deref(),
                                )
                            };
                            match persist_result {
                                Ok(()) => {
                                    info!(
                                        "Persisted audio device selection to {}",
                                        config_path.display()
                                    );
                                    if let Err(e) = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "audio".to_string(),
                                            status: format!(
                                                "Output device set to {} — restart to apply",
                                                output_device.as_deref().unwrap_or("(unchanged)")
                                            ),
                                        },
                                    ) {
                                        debug!("Failed to send device-set status: {}", e);
                                    }
                                }
                                Err(e) => {
                                    warn!("Failed to persist audio device selection: {}", e);
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "audio".to_string(),
                                            status: format!("Failed to save device choice: {}", e),
                                        },
                                    );
                                }
                            }
                        }
                        _ => {
                            debug!("Unhandled TUI command: {:?}", cmd);
                        }
                    },
                    Err(crossbeam_channel::TryRecvError::Empty) => {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(crossbeam_channel::TryRecvError::Disconnected) => break,
                }
            }
            Ok(())
        });
        self.named_task_handles.push((ComponentId::Tui, cmd_handle));

        // Run the TUI on a blocking task (it takes over the terminal)
        let tui_config_lock = config.read().await;
        let tui_config = pancetta_tui::Config {
            station: pancetta_tui::config::StationConfig {
                call_sign: tui_config_lock.station.callsign.clone(),
                grid_square: tui_config_lock.station.grid_square.clone(),
                power: tui_config_lock.station.power_watts,
                antenna: "Vertical".to_string(),
                rig: tui_config_lock.rig.model.clone(),
                default_frequency: 14.074,
                tx_self_parity: tui_config_lock.station.tx_self_parity,
            },
            ui: pancetta_tui::config::UiConfig {
                theme: pancetta_tui::Theme::Dark,
                refresh_rate: 30,
                max_messages: 100,
                show_waterfall: true,
                show_coordinates: true,
                time_format: pancetta_tui::config::TimeFormat::UTC24,
                frequency_format: pancetta_tui::config::FrequencyFormat::MHz,
            },
            audio: pancetta_tui::config::AudioConfig {
                device: Some(tui_config_lock.audio.input_device.clone()),
                sample_rate: tui_config_lock.audio.sample_rate,
                buffer_size: tui_config_lock.audio.buffer_size as usize,
                auto_gain: false,
                gain_level: tui_config_lock.audio.levels.input_gain_db,
            },
            decoder: pancetta_tui::config::DecoderConfig {
                enabled_modes: vec!["FT8".to_string()],
                minimum_snr: -20,
                decode_depth: 3,
                aggressive_decode: true,
                enable_averaging: false,
            },
            bands: pancetta_tui::Config::default().bands,
        };
        drop(tui_config_lock);

        // Start TUI runner in a blocking task so it can own the terminal
        let tui_handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                pancetta_tui::tui_runner::run_tui_with_message_bus(
                    tui_config, tui_msg_rx, tui_cmd_tx, shutdown,
                )
                .await
            })
        });

        // Wrap the JoinHandle and ensure shutdown is triggered when TUI exits
        let tui_shutdown = self.shutdown_signal.clone();
        let tui_wrapper = tokio::spawn(async move {
            let result = match tui_handle.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(e),
                Err(e) => Err(anyhow::anyhow!("TUI task panicked: {}", e)),
            };
            // Always trigger shutdown when TUI exits (user quit, crash, etc.)
            tui_shutdown.store(true, Ordering::Release);
            result
        });
        self.named_task_handles
            .push((ComponentId::Tui, tui_wrapper));

        info!("TUI component started");
        Ok(())
    }
}

/// Map the bus's `AutonomousStatusData` into the TUI's structured
/// `AutonomousStatus`, AND-ing the operator engine's internal `enabled`
/// with the runtime gate (`autonomous_enabled_runtime` — the flag
/// Shift+Q clears and the `a` key re-sets). The panel should show what
/// the station will actually do: when the operator override is active,
/// the qso-crate engine still reports `enabled: true` (it keeps its
/// state so re-enabling picks up cleanly) but no TX will be dispatched,
/// so the TUI must render it as disabled.
/// Map one bus `ActiveQsoSnapshotItem` into the TUI's `ActiveQsoBanner`
/// (decoupled struct so pancetta-tui doesn't link pancetta-qso).
/// Field-for-field copy — the QSO coordinator already derived everything
/// from the state machine; the relay just re-shapes.
fn map_qso_snapshot_item(
    q: &crate::message_bus::ActiveQsoSnapshotItem,
) -> pancetta_tui::app::ActiveQsoBanner {
    pancetta_tui::app::ActiveQsoBanner {
        their_callsign: q.their_callsign.clone(),
        state: q.state.clone(),
        started_at: q.started_at,
        frequency_hz: q.frequency_hz,
        tx_parity: q.tx_parity,
        last_tx_text: q.last_tx_text.clone(),
        last_tx_at: q.last_tx_at,
        last_rx_text: q.last_rx_text.clone(),
        last_rx_at: q.last_rx_at,
        snr_rx: q.snr_rx,
        report_sent: q.report_sent,
        report_received: q.report_received,
        exchange_count: q.exchange_count,
    }
}

/// Worked-before check for TUI enrichment (Batch 95).
///
/// Delegates to `CachedStationLookup::is_duplicate` — the exact method
/// the autonomous priority scorer calls for its duplicate penalty — so
/// the TUI's worked-before flag is consistent with the scorer by
/// construction: band-scoped on `freq_hz`, uppercase-exact match on the
/// full callsign as logged (no /P-style suffix stripping, because the
/// scorer doesn't strip either). `None`/empty callsigns (unparsed
/// decodes) are never "worked".
fn worked_before_for(
    lookup: &crate::priority_evaluator::CachedStationLookup,
    callsign: Option<&str>,
    freq_hz: f64,
) -> bool {
    use pancetta_qso::priority::WorkedStationLookup;
    match callsign {
        Some(c) if !c.is_empty() => lookup.is_duplicate(c, freq_hz),
        _ => false,
    }
}

fn map_autonomous_status(
    data: &crate::message_bus::AutonomousStatusData,
    runtime_gate_open: bool,
) -> pancetta_tui::AutonomousStatus {
    pancetta_tui::AutonomousStatus {
        enabled: data.enabled && runtime_gate_open,
        state: data.state.clone(),
        slot_parity: data.slot_parity.clone(),
        listen_counter: data.listen_counter.clone(),
        active_qsos: data.active_qsos,
        max_qsos: data.max_qsos,
        idle_cycles: data.idle_cycles,
        band_name: data.band_name.clone(),
        tx_offset_hz: data.tx_offset_hz,
    }
}

#[cfg(test)]
mod tui_relay_tests {
    use super::*;

    /// Batch 94: the relay's snapshot→banner mapping must carry every
    /// QSO-detail field through field-for-field — a dropped field here
    /// silently renders as "---" in the panel.
    #[test]
    fn map_qso_snapshot_item_carries_all_detail_fields() {
        let started = chrono::Utc::now() - chrono::Duration::seconds(30);
        let tx_at = started + chrono::Duration::seconds(15);
        let rx_at = started + chrono::Duration::seconds(28);
        let item = crate::message_bus::ActiveQsoSnapshotItem {
            their_callsign: "JA1ABC".to_string(),
            state: "sending rpt".to_string(),
            started_at: started,
            frequency_hz: 1500.0,
            tx_parity: Some(pancetta_core::slot::SlotParity::Odd),
            last_tx_text: Some("JA1ABC K5ARH EM10".to_string()),
            last_tx_at: Some(tx_at),
            last_rx_text: Some("K5ARH JA1ABC -12".to_string()),
            last_rx_at: Some(rx_at),
            snr_rx: Some(-12),
            report_sent: Some(-8),
            report_received: Some(-12),
            exchange_count: 2,
        };
        let banner = map_qso_snapshot_item(&item);
        assert_eq!(banner.their_callsign, "JA1ABC");
        assert_eq!(banner.state, "sending rpt");
        assert_eq!(banner.started_at, started);
        assert_eq!(banner.frequency_hz, 1500.0);
        assert_eq!(banner.tx_parity, Some(pancetta_core::slot::SlotParity::Odd));
        assert_eq!(banner.last_tx_text.as_deref(), Some("JA1ABC K5ARH EM10"));
        assert_eq!(banner.last_tx_at, Some(tx_at));
        assert_eq!(banner.last_rx_text.as_deref(), Some("K5ARH JA1ABC -12"));
        assert_eq!(banner.last_rx_at, Some(rx_at));
        assert_eq!(banner.snr_rx, Some(-12));
        assert_eq!(banner.report_sent, Some(-8));
        assert_eq!(banner.report_received, Some(-12));
        assert_eq!(banner.exchange_count, 2);
    }

    /// Fresh QSO with no traffic yet: None/0 detail fields map through
    /// unchanged (the panel renders placeholders).
    #[test]
    fn map_qso_snapshot_item_handles_empty_details() {
        let item = crate::message_bus::ActiveQsoSnapshotItem {
            their_callsign: "W1AW".to_string(),
            state: "→ called".to_string(),
            started_at: chrono::Utc::now(),
            frequency_hz: 900.0,
            tx_parity: None,
            last_tx_text: None,
            last_tx_at: None,
            last_rx_text: None,
            last_rx_at: None,
            snr_rx: None,
            report_sent: None,
            report_received: None,
            exchange_count: 0,
        };
        let banner = map_qso_snapshot_item(&item);
        assert!(banner.last_tx_text.is_none());
        assert!(banner.last_rx_text.is_none());
        assert!(banner.snr_rx.is_none());
        assert!(banner.report_sent.is_none());
        assert!(banner.report_received.is_none());
        assert_eq!(banner.exchange_count, 0);
    }

    fn sample_status(enabled: bool) -> crate::message_bus::AutonomousStatusData {
        crate::message_bus::AutonomousStatusData {
            enabled,
            state: "Hunting".to_string(),
            slot_parity: Some("Odd".to_string()),
            listen_counter: "3/5".to_string(),
            active_qsos: 2,
            max_qsos: 3,
            idle_cycles: 7,
            band_name: "20m".to_string(),
            tx_offset_hz: 1750.0,
        }
    }

    /// Field-for-field forwarding when both the engine and the runtime
    /// gate agree autonomous is on.
    #[test]
    fn map_forwards_all_fields_when_gate_open() {
        let mapped = map_autonomous_status(&sample_status(true), true);
        assert!(mapped.enabled);
        assert_eq!(mapped.state, "Hunting");
        assert_eq!(mapped.slot_parity.as_deref(), Some("Odd"));
        assert_eq!(mapped.listen_counter, "3/5");
        assert_eq!(mapped.active_qsos, 2);
        assert_eq!(mapped.max_qsos, 3);
        assert_eq!(mapped.idle_cycles, 7);
        assert_eq!(mapped.band_name, "20m");
        assert_eq!(mapped.tx_offset_hz, 1750.0);
    }

    /// After Shift+Q the runtime gate is closed but the engine still
    /// reports enabled=true (it keeps internal state for clean resume).
    /// The TUI must show disabled — that's what the station will do.
    #[test]
    fn map_shows_disabled_while_operator_override_active() {
        let mapped = map_autonomous_status(&sample_status(true), false);
        assert!(
            !mapped.enabled,
            "closed runtime gate must render as disabled"
        );
        // Non-enabled fields still forward so the panel keeps context.
        assert_eq!(mapped.state, "Hunting");
    }

    /// Config-disabled engine stays disabled regardless of the gate.
    #[test]
    fn map_engine_disabled_wins_over_open_gate() {
        let mapped = map_autonomous_status(&sample_status(false), true);
        assert!(!mapped.enabled);
    }

    /// End-to-end gate semantics for the Shift+Q → `a` recovery path,
    /// exercised the same way the command-forwarding loop does it:
    /// emergency stop stores `false`; ToggleAutonomous flips it back.
    /// (The full async loop isn't unit-testable without a live bus,
    /// but the gate IS the seam — both handlers only touch this one
    /// AtomicBool, which the autonomous loop checks before TX.)
    #[test]
    fn emergency_stop_then_toggle_reopens_gate() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let gate = AtomicBool::new(true); // seeded from config.autonomous.enabled

        // Shift+Q → OperatorEmergencyStop handler:
        gate.store(false, Ordering::Release);
        assert!(!gate.load(Ordering::Acquire), "stop must close the gate");

        // `a` → ToggleAutonomous handler:
        let was = gate.load(Ordering::Acquire);
        gate.store(!was, Ordering::Release);
        assert!(
            gate.load(Ordering::Acquire),
            "toggle after stop must reopen the gate (autonomous resumes)"
        );

        // `a` again disables symmetrically.
        let was = gate.load(Ordering::Acquire);
        gate.store(!was, Ordering::Release);
        assert!(!gate.load(Ordering::Acquire));
    }

    /// Batch 95: the TUI's worked-before flag and the autonomous
    /// scorer's duplicate penalty must come from the SAME lookup with
    /// the SAME semantics. Exercise both through the shared
    /// CachedStationLookup and assert they agree on every case.
    #[test]
    fn worked_before_matches_scorer_duplicate_semantics() {
        use pancetta_qso::priority::WorkedStationLookup;
        let lookup = crate::priority_evaluator::CachedStationLookup::new();
        lookup.record_worked("ja1abc", "20m"); // lowercase in, uppercased internally

        let cases = [
            ("JA1ABC", 14_074_000.0),   // worked, same band → true
            ("ja1abc", 14_074_000.0),   // case-insensitive → true
            ("JA1ABC", 7_074_000.0),    // other band → false (band-scoped)
            ("JA1ABC/P", 14_074_000.0), // suffix NOT stripped → false (matches scorer)
            ("DL5XYZ", 14_074_000.0),   // never worked → false
        ];
        for (call, freq) in cases {
            assert_eq!(
                worked_before_for(&lookup, Some(call), freq),
                lookup.is_duplicate(call, freq),
                "TUI and scorer disagree for {} at {} Hz",
                call,
                freq
            );
        }

        // Spot checks on the actual values.
        assert!(worked_before_for(&lookup, Some("JA1ABC"), 14_074_000.0));
        assert!(worked_before_for(&lookup, Some("ja1abc"), 14_074_000.0));
        assert!(!worked_before_for(&lookup, Some("JA1ABC"), 7_074_000.0));
        assert!(!worked_before_for(&lookup, Some("JA1ABC/P"), 14_074_000.0));
    }

    /// Unparsed decodes (no callsign) and empty strings are never
    /// flagged as worked.
    #[test]
    fn worked_before_handles_missing_callsign() {
        let lookup = crate::priority_evaluator::CachedStationLookup::new();
        lookup.record_worked("K1ABC", "20m");
        assert!(!worked_before_for(&lookup, None, 14_074_000.0));
        assert!(!worked_before_for(&lookup, Some(""), 14_074_000.0));
    }

    /// A QSO completing mid-session (record_worked) must flip the flag
    /// for subsequent decodes — the live-update path, not just the
    /// startup seed.
    #[test]
    fn worked_before_updates_live_on_record_worked() {
        let lookup = crate::priority_evaluator::CachedStationLookup::new();
        assert!(!worked_before_for(&lookup, Some("VK2DEF"), 14_074_000.0));
        lookup.record_worked("VK2DEF", "20m");
        assert!(worked_before_for(&lookup, Some("VK2DEF"), 14_074_000.0));
    }
}
