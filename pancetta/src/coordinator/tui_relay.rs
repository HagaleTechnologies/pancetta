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
    // rationale: wires many independent channel endpoints and shared handles into
    // the TUI task; a params struct would just relocate the same fields.
    #[allow(clippy::too_many_arguments)]
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
        // Operator-presence clock shared with the autonomous-initiation gate
        // (FCC §97.221): the TUI stamps it on every keypress.
        let tui_last_input = self.last_operator_input_ms.clone();

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
        // Rig-connection + TX-output-misconfig badges (read on the 2s health
        // tick; pushed only on change so the TUI render stays cheap).
        let rig_conn_state_relay = self.rig_conn_state.clone();
        let audio_output_default_relay = self.audio_output_default.clone();
        let tui_relay_jh = std::thread::Builder::new()
            .name("tui-relay".to_string())
            .spawn(move || {
            let mut ft8_disconnected = false;
            let mut last_health_send = std::time::Instant::now();
            // Last-pushed badge state, so we only emit on change (and force the
            // first push by seeding sentinels that differ from any real value).
            let mut last_rig_state: Option<u8> = None;
            let mut last_audio_default: Option<bool> = None;
            // C20 — RF-present / zero-decodes detector (mode/clock fault),
            // fed from the cumulative DSP-window + decode telemetry below.
            let mut rf_no_decode = super::health::RfNoDecodeMonitor::new();
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
                            let (needed, atno) =
                                needed_atno_for(&relay_station_lookup, call_sign.as_deref());

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
                                needed,
                                atno,
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
                if let Ok(bus_msg) = tui_bus_rx.try_recv() {
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
                        MessageType::TxQueueStatus {
                            ref sending,
                            ref queued,
                        } => {
                            // Richer NOW-SENDING / QUEUED view. Re-shape the
                            // coordinator's TxItem into the TUI's local
                            // TxQueueItem (decoupled so the TUI doesn't link
                            // the main crate).
                            let map = |it: &crate::message_bus::TxItem| {
                                pancetta_tui::app::TxQueueItem {
                                    text: it.text.clone(),
                                    freq_hz: it.freq_hz,
                                    qso_id: it.qso_id.clone(),
                                    deferred: it.deferred,
                                }
                            };
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::TxQueueUpdate {
                                    sending: sending.as_ref().map(map),
                                    queued: queued.iter().map(map).collect(),
                                },
                            );
                        }
                        MessageType::TxPolicyStatus { policy } => {
                            // Echo the global TX policy to the bold banner.
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::TxPolicyUpdate { policy },
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
                            let (needed, atno) =
                                needed_atno_for(&relay_station_lookup, Some(callsign.as_str()));
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DxSpot {
                                    callsign,
                                    frequency,
                                    spotter,
                                    worked_before,
                                    needed,
                                    atno,
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

                // Relay waterfall data from FT8 decoder to TUI
                if let Ok(rows) = waterfall_rx.try_recv() {
                    let _ = tui_msg_tx_relay
                        .send(pancetta_tui::tui_runner::TuiMessage::WaterfallUpdate { rows });
                }

                // Relay audio level from DSP to TUI
                if let Ok(level) = audio_level_rx.try_recv() {
                    let _ = tui_msg_tx_relay
                        .send(pancetta_tui::tui_runner::TuiMessage::AudioLevel { level });
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

                    // Rig-connection badge — push only when it changes.
                    let rig_u8 = rig_conn_state_relay.load(Ordering::Relaxed);
                    if last_rig_state != Some(rig_u8) {
                        last_rig_state = Some(rig_u8);
                        let state = match super::hamlib::RigConnState::from_u8(rig_u8) {
                            super::hamlib::RigConnState::Connected => {
                                pancetta_tui::app::RigConnDisplay::Connected
                            }
                            super::hamlib::RigConnState::PollingFailed => {
                                pancetta_tui::app::RigConnDisplay::PollingFailed
                            }
                            super::hamlib::RigConnState::NotConnected => {
                                pancetta_tui::app::RigConnDisplay::NotConnected
                            }
                        };
                        let _ = tui_msg_tx_relay.send(
                            pancetta_tui::tui_runner::TuiMessage::RigStatusUpdate { state },
                        );
                    }

                    // TX-output misconfig badge — push only when it changes.
                    let audio_default = audio_output_default_relay.load(Ordering::Relaxed);
                    if last_audio_default != Some(audio_default) {
                        last_audio_default = Some(audio_default);
                        let _ = tui_msg_tx_relay.send(
                            pancetta_tui::tui_runner::TuiMessage::AudioOutputDefault {
                                is_default: audio_default,
                            },
                        );
                    }

                    // C20 — RF present but zero decodes over several slots →
                    // likely wrong mode (FT8/FT4) or a bad system clock. Feed
                    // the cumulative DSP-window + decode counters and the latest
                    // RMS; emit an operator status only on a warn on/off edge.
                    let rf_dsp_windows = health_dsp_windows_relay.load(Ordering::Relaxed);
                    let rf_total_decodes =
                        health_total_decodes_relay.load(Ordering::Relaxed);
                    let rf_last_rms =
                        f32::from_bits(health_last_rms_relay.load(Ordering::Relaxed));
                    let edges =
                        rf_no_decode.observe(rf_dsp_windows, rf_total_decodes, rf_last_rms);
                    match edges.rf_no_decode {
                        Some(true) => {
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "dsp".to_string(),
                                    status: "⚠ RF present but no decodes — check mode/clock?"
                                        .to_string(),
                                },
                            );
                        }
                        Some(false) => {
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "dsp".to_string(),
                                    status: "Decodes resumed — RF/no-decode warning cleared"
                                        .to_string(),
                                },
                            );
                        }
                        None => {}
                    }
                    // Silent-input warning: the stream is running but the
                    // samples are ~0 (muted/missing device, denied mic
                    // permission, or a remote-desktop client holding the
                    // CODEC). Distinct from a quiet-but-live band.
                    match edges.silent_input {
                        Some(true) => {
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "audio".to_string(),
                                    status: "⚠ INPUT SILENT (RMS≈0) — check Sound input device, \
                                             mic permission, and that nothing else grabbed the CODEC"
                                        .to_string(),
                                },
                            );
                        }
                        Some(false) => {
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "audio".to_string(),
                                    status: "Audio input restored — silence warning cleared"
                                        .to_string(),
                                },
                            );
                        }
                        None => {}
                    }
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
        // C9 dedup anchor: record that *pancetta* (via the operator's TUI
        // SetFrequency) commanded this dial change, so the hamlib poll loop
        // doesn't double-fire the teardown when it reads the new freq back.
        let cmd_last_freq_command = self.last_freq_command.clone();
        // (CQ text is no longer generated in this task — the CallingCq QSO in
        // the QSO component owns it, rendered from the operator's configured
        // callsign/grid there.)
        let cmd_ptt_state = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cmd_cq_active = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let cmd_abort_current_tx = self.abort_current_tx.clone();
        let cmd_autonomous_enabled = self.autonomous_enabled_runtime.clone();
        // Global tri-state TX policy (Full/RespondOnly/Disabled). The
        // command handler updates this on CycleTxPolicy and Shift+Q, gates
        // initiation commands (StartCq, CallStation) on it, and echoes the
        // resulting state back to the TUI banner.
        let cmd_tx_policy = self.tx_policy.clone();
        // Operator Hold/Auto TX-frequency mode (`f`). The handler toggles this
        // atomic; the QSO engine and autonomous operator read it to gate
        // autonomous frequency moves.
        let cmd_tx_freq_mode = self.tx_freq_mode.clone();
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
        // Live device-switch channel into the audio thread. `None` in
        // stub/`--no-audio` modes — the SelectDevice handler then persists the
        // choice (applies on next restart) and tells the operator it can't apply
        // live in this mode.
        let cmd_audio_reopen_tx = self.audio_reopen_tx.clone();
        // F4 toggle state: Some(t) when a tune is in flight and expected
        // to auto-stop at instant t. None when no tune is queued. The
        // coordinator owns this — TUI just emits ToggleTune events.
        let cmd_tune_until: std::sync::Arc<tokio::sync::RwLock<Option<tokio::time::Instant>>> =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));
        // Non-fatal config-load warnings to surface to the TUI as an error
        // banner once at startup (e.g. a pancetta.toml that failed to parse
        // and silently reverted to defaults).
        let cmd_config_warnings = self.config_warnings.clone();
        const TUNE_DURATION_SECS: u32 = 12;
        const TUNE_TONE_HZ: f64 = 1500.0;
        let cmd_handle = tokio::spawn(async move {
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

            // Seed the TX-policy banner so it is authoritative from frame 1.
            // The banner otherwise defaults to TxPolicy::default() and is only
            // ever corrected on an explicit operator change — push the real
            // atomic value once at startup so a non-default seeded policy is
            // shown correctly.
            {
                let policy =
                    pancetta_core::TxPolicy::from_u8(cmd_tx_policy.load(Ordering::Acquire));
                let _ = cmd_tui_msg_tx
                    .send(pancetta_tui::tui_runner::TuiMessage::TxPolicyUpdate { policy });
            }

            // Surface any non-fatal config-load warnings to the operator as an
            // error banner (the same path audio-init failures use). A partial
            // or broken pancetta.toml silently reverting to defaults is exactly
            // the trap this closes — the operator now sees it in the TUI.
            for w in &cmd_config_warnings {
                let _ = cmd_tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::Error {
                    component: "config".to_string(),
                    message: w.clone(),
                });
            }

            while !cmd_shutdown.load(Ordering::Acquire) {
                // CQ is now TRANSMITTED by a real CallingCq QSO owned by the
                // QSO component (StartCq → QsoManager::start_cq_manual, which
                // keep-calls every slot and auto-sequences the exchange to
                // completion when a station answers). This task no longer
                // transmits CQ text itself — that would be a SECOND CQ TX
                // source on the same slot/freq (double-TX). `cmd_cq_active` is
                // kept purely as bookkeeping (Shift+Q / F8 / the policy-stop
                // below read it).
                //
                // CQ is an initiation: if the operator cycled the TX policy
                // away from Full while a CQ was running, stop it — clear the
                // active flag (so it doesn't silently resume on return to
                // Full; the operator must re-press `c`) AND cancel the
                // CallingCq QSO in the QSO component so it stops keep-calling.
                if cmd_cq_active.load(Ordering::Relaxed) {
                    let policy =
                        pancetta_core::TxPolicy::from_u8(cmd_tx_policy.load(Ordering::Acquire));
                    if !policy.allows_initiation() {
                        info!(
                            target: "tx.policy",
                            "Stopping manual CQ: TX policy is now {}",
                            policy.label()
                        );
                        cmd_cq_active.store(false, Ordering::Relaxed);
                        let msg = ComponentMessage::new(
                            ComponentId::Tui,
                            ComponentId::Qso,
                            MessageType::QsoMessage(crate::message_bus::QsoMessage::StopCq),
                            Instant::now(),
                        );
                        if let Err(e) = cmd_message_bus.send_message(msg).await {
                            warn!("Failed to cancel CQ QSO on policy change: {}", e);
                        }
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
                            // CallStation initiates a NEW contact with a CQer
                            // (DX-hunter pounce). Gated by the TX policy: only
                            // Full permits initiation. RespondOnly/Disabled
                            // refuse and warn the operator.
                            let policy = pancetta_core::TxPolicy::from_u8(
                                cmd_tx_policy.load(Ordering::Acquire),
                            );
                            if !policy.allows_initiation() {
                                warn!(
                                    target: "tx.policy",
                                    "Refusing CallStation {} ({} Hz): TX policy is {} \
                                     (initiation disallowed)",
                                    callsign, frequency, policy.label()
                                );
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "TX".to_string(),
                                        status: format!(
                                            "Can't call {} — TX policy is {} (press g for Full)",
                                            callsign,
                                            policy.label()
                                        ),
                                    },
                                );
                                continue;
                            }
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
                        pancetta_tui::tui_runner::TuiCommand::RespondToCaller {
                            callsign,
                            frequency,
                            dx_parity,
                            step,
                            snr,
                        } => {
                            info!(
                                "TUI RespondToCaller: {} at {} Hz (step {:?})",
                                callsign, frequency, step
                            );
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(
                                    crate::message_bus::QsoMessage::RespondToCaller {
                                        callsign,
                                        frequency,
                                        dx_parity,
                                        step,
                                        snr,
                                    },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward RespondToCaller command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::AbortQso { qso_id } => {
                            info!("TUI AbortQso: {}", qso_id);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(crate::message_bus::QsoMessage::AbortQso {
                                    qso_id,
                                }),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward AbortQso command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::ResendQso { qso_id } => {
                            info!("TUI ResendQso: {}", qso_id);
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(
                                    crate::message_bus::QsoMessage::ResendQso { qso_id },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward ResendQso command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::SetFrequency { vfo, frequency } => {
                            info!("TUI SetFrequency: VFO {} -> {} Hz", vfo, frequency);
                            // C9 — band change mid-QSO: tear down active QSOs so no
                            // stale keep-call keeps TXing on the new band. Capture the
                            // *old* dial frequency BEFORE we overwrite the atomic, then
                            // decide whether this dial move is a genuine band change.
                            let old_freq_hz = cmd_operating_freq_hz.load(Ordering::Relaxed);
                            let freq_mhz = frequency as f64 / 1_000_000.0;
                            cmd_operating_freq.store(freq_mhz.to_bits(), Ordering::Relaxed);
                            cmd_operating_freq_hz.store(frequency, Ordering::Relaxed);
                            // Stamp the C9 dedup anchor: pancetta commanded this
                            // freq, so the hamlib poll loop suppresses its own
                            // teardown (here and during the rig settle window).
                            if let Ok(mut anchor) = cmd_last_freq_command.lock() {
                                *anchor = Some((frequency, Instant::now()));
                            }
                            if super::is_band_change(old_freq_hz, frequency) {
                                info!(
                                    target: "operator.override",
                                    "Band change {} Hz -> {} Hz — tearing down active QSOs",
                                    old_freq_hz, frequency
                                );
                                let teardown = ComponentMessage::new(
                                    ComponentId::Tui,
                                    ComponentId::Qso,
                                    MessageType::QsoMessage(
                                        crate::message_bus::QsoMessage::BandChanged {
                                            previous_hz: old_freq_hz,
                                            new_hz: frequency,
                                        },
                                    ),
                                    Instant::now(),
                                );
                                if let Err(e) = cmd_message_bus.send_message(teardown).await {
                                    warn!("Band change: failed to send teardown: {}", e);
                                }
                            }
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
                            // Calling CQ is an initiation. Gated by TX policy:
                            // only Full permits it.
                            let policy = pancetta_core::TxPolicy::from_u8(
                                cmd_tx_policy.load(Ordering::Acquire),
                            );
                            if !policy.allows_initiation() {
                                warn!(
                                    target: "tx.policy",
                                    "Refusing StartCq: TX policy is {} (initiation disallowed)",
                                    policy.label()
                                );
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "TX".to_string(),
                                        status: format!(
                                            "Can't start CQ — TX policy is {} (press g for Full)",
                                            policy.label()
                                        ),
                                    },
                                );
                                continue;
                            }
                            info!(
                                "TUI StartCq: starting manual CQ QSO at {:.0} Hz (waterfall \
                                 cursor)",
                                frequency_offset
                            );
                            // Bookkeeping only — the CQ is TRANSMITTED by a real
                            // CallingCq QSO owned by the QSO component (below),
                            // NOT the old text-only loop in this task, so there
                            // is exactly one CQ TX source per slot (no
                            // double-TX). The QSO keep-calls every slot and,
                            // when a station answers, auto-sequences the
                            // exchange to Completed + ADIF log.
                            cmd_cq_active.store(true, Ordering::Relaxed);
                            // tx_parity = None: calling CQ we choose our own slot
                            // parity; let the TX scheduler resolve it via the
                            // configured self-parity fallback (consistent with
                            // QsoManager::start_cq's default).
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(crate::message_bus::QsoMessage::StartCq {
                                    frequency: frequency_offset.round().max(0.0) as u64,
                                    tx_parity: None,
                                }),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward StartCq command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::StopCq => {
                            info!("TUI StopCq: stopping manual CQ QSO");
                            cmd_cq_active.store(false, Ordering::Relaxed);
                            // Cancel the un-answered CallingCq QSO in the QSO
                            // component (an already-answered exchange is left to
                            // finish).
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(crate::message_bus::QsoMessage::StopCq),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward StopCq command: {}", e);
                            }
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
                            // Emergency stop also hard-mutes all TX: set the
                            // global policy to Disabled (RX-only) and echo it
                            // to the TUI banner. The operator restores TX with
                            // the policy cycle key (`g`).
                            cmd_tx_policy.store(
                                pancetta_core::TxPolicy::Disabled.as_u8(),
                                Ordering::Release,
                            );
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::TxPolicyUpdate {
                                    policy: pancetta_core::TxPolicy::Disabled,
                                },
                            );
                            // Clear the TX *source*, not just mute it: cancel
                            // every active QSO so manual keep-calling (and any
                            // duplicate QSO objects) stops re-emitting each slot.
                            // Without this, returning the policy to Full would
                            // resume the runaway. This is the real fix for
                            // "h + k didn't stop it; only restart did."
                            let cancel_all = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(
                                    crate::message_bus::QsoMessage::CancelAllQsos,
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(cancel_all).await {
                                warn!("Emergency stop: failed to send CancelAllQsos: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::CycleTxPolicy => {
                            // Operator pressed `g`: cycle the global TX policy
                            // Full → RespondOnly → Disabled → Full. Update the
                            // shared atomic and echo the new state to the TUI
                            // banner (mirrors ToggleAutonomous's echo pattern).
                            let prev = pancetta_core::TxPolicy::from_u8(
                                cmd_tx_policy.load(Ordering::Acquire),
                            );
                            let next = prev.cycle();
                            cmd_tx_policy.store(next.as_u8(), Ordering::Release);
                            warn!(
                                target: "tx.policy",
                                "Operator cycled global TX policy: {} -> {}",
                                prev.label(),
                                next.label()
                            );
                            // Cycling to Disabled must abort the CURRENT
                            // transmission, not just gate the next one. Set the
                            // same abort flag Shift+Q uses so the in-flight TX
                            // (up to 12.64s of FT8, or an active tune) stops
                            // within ~50ms via the worker's interruptible_sleep.
                            // Also cancel the manual CQ QSO so it stops
                            // keep-calling (clearing the bookkeeping flag alone
                            // no longer stops TX — the QSO owns it now).
                            if next == pancetta_core::TxPolicy::Disabled {
                                cmd_abort_current_tx.store(true, Ordering::Release);
                                cmd_cq_active.store(false, Ordering::Relaxed);
                                *cmd_tune_until.write().await = None;
                                let msg = ComponentMessage::new(
                                    ComponentId::Tui,
                                    ComponentId::Qso,
                                    MessageType::QsoMessage(crate::message_bus::QsoMessage::StopCq),
                                    Instant::now(),
                                );
                                if let Err(e) = cmd_message_bus.send_message(msg).await {
                                    warn!("Failed to cancel CQ QSO on TX disable: {}", e);
                                }
                            }
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::TxPolicyUpdate {
                                    policy: next,
                                },
                            );
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "TX".to_string(),
                                    status: format!("TX policy: {}", next.label()),
                                },
                            );
                        }
                        pancetta_tui::tui_runner::TuiCommand::ToggleTxFreqMode => {
                            // Operator pressed `f`: toggle the TX-frequency mode
                            // Hold ↔ Auto. Hold (default) keeps the operator's
                            // picked offset sticky; Auto lets pancetta choose and
                            // adjust it (smart allocator + collision jitter +
                            // stuck-DX hop). Update the shared atomic; the TUI
                            // chip is driven optimistically on the key side.
                            let prev = pancetta_core::TxFreqMode::from_u8(
                                cmd_tx_freq_mode.load(Ordering::Acquire),
                            );
                            let next = prev.toggle();
                            cmd_tx_freq_mode.store(next.as_u8(), Ordering::Release);
                            info!(
                                target: "tx.freq",
                                "Operator toggled TX-frequency mode: {} -> {}",
                                prev.label(),
                                next.label()
                            );
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "TX".to_string(),
                                    status: format!("TX freq: {}", next.label()),
                                },
                            );
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
                            // Also cancel the manual CQ QSO so we don't
                            // immediately re-arm a new CQ TX next slot.
                            // Clear the tune-until tracker so the F4 toggle
                            // re-arms cleanly next press.
                            info!("TUI StopTx: halting current TX (F8)");
                            cmd_abort_current_tx.store(true, Ordering::Release);
                            cmd_cq_active.store(false, Ordering::Relaxed);
                            *cmd_tune_until.write().await = None;
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(crate::message_bus::QsoMessage::StopCq),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to cancel CQ QSO on StopTx: {}", e);
                            }
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
                                // TX-policy safety gate: starting a tune puts a
                                // carrier on the air. Refuse it when the global
                                // policy is Disabled (RX-only). (Aborting an
                                // in-flight tune above is always allowed.)
                                let policy = pancetta_core::TxPolicy::from_u8(
                                    cmd_tx_policy.load(Ordering::Acquire),
                                );
                                if !policy.allows_any_tx() {
                                    warn!(
                                        target: "tx.policy",
                                        "Refusing tune start: TX policy is {} (RX-only)",
                                        policy.label()
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: "Can't tune — TX is DISABLED (press g \
                                                     to re-enable)"
                                                .to_string(),
                                        },
                                    );
                                    continue;
                                }
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
                            // TX-policy safety gate: a PTT key-UP (state=true)
                            // is a transmission. Refuse it when the global
                            // policy is Disabled (RX-only) — keying the rig
                            // there would put a carrier on the air after the
                            // operator hit Shift+Q / cycled to Disabled.
                            // PTT-OFF (state=false) is ALWAYS allowed: it can
                            // only ever stop TX, never start it.
                            if new_state {
                                let policy = pancetta_core::TxPolicy::from_u8(
                                    cmd_tx_policy.load(Ordering::Acquire),
                                );
                                if !policy.allows_any_tx() {
                                    warn!(
                                        target: "tx.policy",
                                        "Refusing PTT key-up: TX policy is {} (RX-only)",
                                        policy.label()
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: "Can't key PTT — TX is DISABLED (press g \
                                                     to re-enable)"
                                                .to_string(),
                                        },
                                    );
                                    continue;
                                }
                            }
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
                            // survives a restart, AND apply it live by asking the
                            // audio thread to reopen the cpal stream(s) on the new
                            // device(s) — no restart required.
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
                            if let Err(e) = persist_result {
                                warn!("Failed to persist audio device selection: {}", e);
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "audio".to_string(),
                                        status: format!("Failed to save device choice: {}", e),
                                    },
                                );
                            } else {
                                info!(
                                    "Persisted audio device selection to {}",
                                    config_path.display()
                                );
                            }

                            // Apply LIVE: ask the audio thread to reopen the
                            // cpal stream(s) on the new device(s) without a
                            // restart, and relay the outcome to the operator.
                            // Prefer the output name in the status text (the
                            // common picker action); fall back to input.
                            let picked = output_device
                                .clone()
                                .or_else(|| input_device.clone())
                                .unwrap_or_else(|| "(unchanged)".to_string());
                            match cmd_audio_reopen_tx {
                                Some(ref reopen_tx) => {
                                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                    let req = crate::coordinator::audio::AudioReopenRequest {
                                        input: input_device.clone(),
                                        output: output_device.clone(),
                                        // Explicit operator pick → force a rebuild even
                                        // if the name is unchanged, so re-selecting the
                                        // rig device reclaims it from a hijacker.
                                        force: true,
                                        respond: resp_tx,
                                    };
                                    let status = if reopen_tx.send(req).is_err() {
                                        warn!(
                                            "Audio reopen channel closed; device not switched live"
                                        );
                                        format!(
                                            "Device {} saved — live switch unavailable (audio thread gone); restart to apply",
                                            picked
                                        )
                                    } else {
                                        // Bound the wait so a wedged audio thread
                                        // can't hang the TUI command loop.
                                        match tokio::time::timeout(Duration::from_secs(5), resp_rx)
                                            .await
                                        {
                                            Ok(Ok(Ok(()))) => {
                                                info!(
                                                    "Live audio device switch succeeded: {}",
                                                    picked
                                                );
                                                format!("Device → {} (live)", picked)
                                            }
                                            Ok(Ok(Err(err))) => {
                                                warn!("Live audio device switch failed: {}", err);
                                                format!(
                                                    "Failed to switch to {} ({}) — kept previous device",
                                                    picked, err
                                                )
                                            }
                                            Ok(Err(_)) => {
                                                warn!("Audio thread dropped reopen response");
                                                format!(
                                                    "Device {} saved — no response from audio thread; restart to apply",
                                                    picked
                                                )
                                            }
                                            Err(_) => {
                                                warn!("Live audio device switch timed out");
                                                format!(
                                                    "Device {} saved — live switch timed out; restart to apply",
                                                    picked
                                                )
                                            }
                                        }
                                    };
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "audio".to_string(),
                                            status,
                                        },
                                    );
                                }
                                None => {
                                    // Stub / --no-audio: no live stream to reopen.
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "audio".to_string(),
                                            status: format!(
                                                "Device {} saved — restart to apply (no live audio in this mode)",
                                                picked
                                            ),
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
                    tui_config,
                    tui_msg_rx,
                    tui_cmd_tx,
                    shutdown,
                    tui_last_input,
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
        qso_id: q.qso_id.clone(),
        initiated_by: q.initiated_by.clone(),
        ladder_labels: q.ladder_labels.clone(),
        ladder_ours: q.ladder_ours.clone(),
        ladder_index: q.ladder_index,
        now_line: q.now_line.clone(),
        next_line: q.next_line.clone(),
        call_count: q.call_count,
        max_calls: q.max_calls,
        watchdog_deadline: q.watchdog_deadline,
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

/// Compute `(needed, atno)` for a callsign against the same
/// `CachedStationLookup` the autonomous scorer consults. Both inert
/// (false) when cqdx supplies no needed set, or the callsign is absent.
/// `atno` implies `needed`.
fn needed_atno_for(
    lookup: &crate::priority_evaluator::CachedStationLookup,
    callsign: Option<&str>,
) -> (bool, bool) {
    use pancetta_qso::priority::WorkedStationLookup;
    match callsign {
        Some(c) if !c.is_empty() => (lookup.is_needed_dxcc(c), lookup.is_atno(c)),
        _ => (false, false),
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
            qso_id: "11111111-1111-1111-1111-111111111111".to_string(),
            initiated_by: "Manual".to_string(),
            ladder_labels: vec!["Grid".to_string(), "Rpt".to_string()],
            ladder_ours: vec![true, false],
            ladder_index: 1,
            now_line: "waiting".to_string(),
            next_line: "their signal report".to_string(),
            call_count: 4,
            max_calls: 10,
            watchdog_deadline: Some(started + chrono::Duration::minutes(5)),
        };
        let banner = map_qso_snapshot_item(&item);
        assert_eq!(banner.qso_id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(banner.initiated_by, "Manual");
        assert_eq!(banner.ladder_index, 1);
        assert_eq!(banner.now_line, "waiting");
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
        // Batch 2 #1: watchdog fields carry through.
        assert_eq!(banner.call_count, 4);
        assert_eq!(banner.max_calls, 10);
        assert_eq!(
            banner.watchdog_deadline,
            Some(started + chrono::Duration::minutes(5))
        );
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
            qso_id: "22222222-2222-2222-2222-222222222222".to_string(),
            initiated_by: "Auto".to_string(),
            ladder_labels: Vec::new(),
            ladder_ours: Vec::new(),
            ladder_index: 0,
            now_line: String::new(),
            next_line: String::new(),
            call_count: 0,
            max_calls: 0,
            watchdog_deadline: None,
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
