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

        // Relay decoded messages from FT8 -> TUI on a dedicated thread
        // (tokio::spawn was causing starvation -- same pattern as DSP/FT8 fixes)
        let relay_shutdown = shutdown.clone();
        let tui_msg_tx_relay = tui_msg_tx.clone();
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

                            let tui_decoded = pancetta_tui::DecodedMessageView {
                                timestamp: chrono::Utc::now(),
                                frequency: f64::from_bits(
                                    operating_freq_relay.load(Ordering::Relaxed),
                                ),
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
                                // Forward as status update for now
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "Autonomous".to_string(),
                                        status: status.state.clone(),
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
                            MessageType::DxMessage(crate::message_bus::DxMessage::Spot {
                                callsign,
                                frequency,
                                spotter,
                                ..
                            }) => {
                                let _ = tui_msg_tx_relay.send(
                                    pancetta_tui::tui_runner::TuiMessage::DxSpot {
                                        callsign,
                                        frequency,
                                        spotter,
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
        let cmd_handle = tokio::spawn(async move {
            let mut next_cq_time: Option<tokio::time::Instant> = None;

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
                                frequency_offset: 1500.0,
                                qso_id: None,
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
                        pancetta_tui::tui_runner::TuiCommand::SendMessage { text } => {
                            info!("TUI SendMessage: '{}'", text);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Ft8Transmitter,
                                MessageType::TransmitRequest {
                                    message_text: text,
                                    frequency_offset: 1500.0,
                                    qso_id: None,
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
                        } => {
                            info!("TUI CallStation: {} at {} Hz", callsign, frequency);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(crate::message_bus::QsoMessage::StartQso {
                                    callsign,
                                    frequency,
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
                        pancetta_tui::tui_runner::TuiCommand::StartCq => {
                            info!("TUI StartCq: enabling repeating CQ");
                            cmd_cq_active.store(true, Ordering::Relaxed);
                            // Send first CQ immediately by resetting the timer
                            next_cq_time = None;
                        }
                        pancetta_tui::tui_runner::TuiCommand::StopCq => {
                            info!("TUI StopCq: stopping repeating CQ");
                            cmd_cq_active.store(false, Ordering::Relaxed);
                            next_cq_time = None;
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
