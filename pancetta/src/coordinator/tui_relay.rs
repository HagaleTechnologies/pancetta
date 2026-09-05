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
        ft8_to_tui_rx: crossbeam_channel::Receiver<super::pipeline::RelayedDecode>,
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

        // Display priority scorer: read the operator's configured weights
        // once at relay startup (they don't hot-reload at runtime), then
        // build a `PriorityScorer` inside the relay thread so every decode
        // carries a nuanced f64 score from the REAL scorer rather than the
        // coarse 0/500/1000 bucket function. The lookup clone shares the
        // same Arc<RwLock> internals as relay_station_lookup, so it always
        // sees the latest needed/rarity/worked data.
        let relay_priority_weights = {
            let cfg = self.config.read().await;
            let p = &cfg.autonomous.priorities;
            pancetta_qso::priority::PriorityWeights {
                needed_dxcc: p.needed_dxcc,
                needed_grid: p.needed_grid,
                pota_sota: p.pota_sota,
                rarity: p.rarity,
                signal_strength: p.signal_strength,
                duplicate_penalty: p.duplicate_penalty,
                recent_failure_penalty: p.recent_failure_penalty,
                atno_bonus: p.atno_bonus,
            }
        };
        // Clone the lookup for the scorer (Arc-fields share the live data).
        let relay_scorer_lookup = (*self.cached_lookup).clone();

        // Station-wide active operating mode atomic — cloned here so the
        // per-decode mode stamping below reads the LIVE mode (a runtime
        // Shift+M switch takes effect on the very next decode), not a
        // one-time snapshot taken at relay startup.
        let relay_active_protocol_mode = self.active_protocol_mode();

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
        // decoder-speed-overhaul Task 12: decode-budget metrics, read on the
        // same 2s health tick and folded into `PipelineHealth` below.
        let decode_last_elapsed_ms_relay = self.decode_last_elapsed_ms.clone();
        let decode_last_budget_exhausted_relay = self.decode_last_budget_exhausted.clone();
        // Rig-connection + TX-output-misconfig badges (read on the 2s health
        // tick; pushed only on change so the TUI render stays cheap).
        let rig_conn_state_relay = self.rig_conn_state.clone();
        let audio_output_default_relay = self.audio_output_default.clone();
        let audio_input_fallback_relay = self.audio_input_fallback.clone();
        // Clone our callsign before the tui-relay thread consumes the original
        // via move. The async command-handler task below needs its own copy.
        let cmd_our_callsign = our_callsign_for_relay.clone();
        let tui_relay_jh = std::thread::Builder::new()
            .name("tui-relay".to_string())
            .spawn(move || {
            let mut ft8_disconnected = false;
            let mut last_health_send = std::time::Instant::now();
            // Build the display priority scorer once per relay thread.
            // Uses the same weights and lookup (via Arc-shared internals)
            // as the autonomous scorer, but the DX Hunter's "Pri" column now
            // reflects the #164 tiered score (0-5999, strict tier dominance
            // — see TieredScore::as_display_u32) rather than the old
            // continuous [0,1] mapped to [0,1000].
            let relay_scorer = pancetta_qso::PriorityScorer::new(
                relay_priority_weights,
                Box::new(relay_scorer_lookup),
            );
            // Last-pushed badge state, so we only emit on change (and force the
            // first push by seeding sentinels that differ from any real value).
            let mut last_rig_state: Option<u8> = None;
            let mut last_audio_default: Option<bool> = None;
            let mut last_input_fallback: Option<bool> = None;
            // C20 — RF-present / zero-decodes detector (mode/clock fault),
            // fed from the cumulative DSP-window + decode telemetry below.
            let mut rf_no_decode = super::health::RfNoDecodeMonitor::new();
            while !relay_shutdown.load(Ordering::Acquire) {
                if !ft8_disconnected {
                    match ft8_to_tui_rx.try_recv() {
                        Ok(relayed) => {
                            let decoded_msg = &relayed.message;
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
                            let dial_mhz = decode_view_dial_mhz(&relayed);
                            let worked_before = worked_before_for(
                                &relay_station_lookup,
                                call_sign.as_deref(),
                                dial_mhz * 1_000_000.0,
                            );
                            let (needed, atno) =
                                needed_atno_for(&relay_station_lookup, call_sign.as_deref());
                            let band_needed = dxcc_needed_on_band_for(
                                &relay_station_lookup,
                                call_sign.as_deref(),
                                dial_mhz * 1_000_000.0,
                            );

                            // Compute the #164 tiered priority score via the
                            // real PriorityScorer's classification (ATNO >
                            // per-band-DXCC-new > special-station >
                            // per-band-grid-new > everything else, encoded
                            // to a single sortable u32 — see
                            // TieredScore::as_display_u32). Only meaningful
                            // for CQ frames that carry a callsign; non-CQ
                            // decodes (RR73/73/reports) get the same score
                            // but it won't influence the DX Hunter because
                            // only CQ frames are listed there.
                            let priority_score = call_sign.as_deref().map(|cs| {
                                let freq_hz = dial_mhz * 1_000_000.0;
                                let snr_i8 = decoded_msg.snr_db.round().clamp(-128.0, 127.0) as i8;
                                relay_scorer
                                    .score_tiered(cs, grid_square.as_deref(), snr_i8, freq_hz)
                                    .as_display_u32()
                            });

                            let tui_decoded = pancetta_tui::DecodedMessageView {
                                timestamp: chrono::Utc::now(),
                                frequency: dial_mhz,
                                mode: super::mode_str(pancetta_config::OperatingMode::from_u8(
                                    relay_active_protocol_mode.load(Ordering::Relaxed),
                                ))
                                .to_string(),
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
                                band_needed,
                                priority_score,
                                is_own_tx: false,
                            };

                            match tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DecodedMessage(tui_decoded),
                            ) {
                                // Perf (Pass 1): one decode forwards dozens of
                                // these per slot — demote to debug so steady-state
                                // info logging isn't dominated by per-decode spam.
                                Ok(()) => debug!("TUI relay: forwarded decoded message to TUI channel"),
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
                        MessageType::TxFrameLogged {
                            text,
                            freq_hz,
                            qso_id,
                            timestamp,
                        } => {
                            // #172: pass-through relay for Band Activity's
                            // own-TX history, same shape as QsoHistoryEntry.
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::TxFrameLogged {
                                    text,
                                    freq_hz,
                                    qso_id,
                                    timestamp,
                                },
                            );
                        }
                        MessageType::TxPolicyStatus { policy } => {
                            // Echo the global TX policy to the bold banner.
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::TxPolicyUpdate { policy },
                            );
                        }
                        MessageType::DecodeEffortStatus { effort, budget_ms } => {
                            // Echo the live decode-effort preset to the
                            // "DECODE: <PRESET> <ms>ms" title-bar chip
                            // (decoder-speed-overhaul Task 15).
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DecodeEffortUpdate {
                                    effort,
                                    budget_ms,
                                },
                            );
                        }
                        MessageType::SplitStatus { tx_hz } => {
                            // Echo the authoritative split TX state to the
                            // title-bar chip. Sent from all three split-atomic
                            // write sites (SetSplit command, manual band-change,
                            // autonomous band-hop).
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::SplitUpdate { tx_hz },
                            );
                        }
                        MessageType::TxOffsetStatus { offset_hz } => {
                            // Whole-branch-review fix (Task 16 auto-repark):
                            // the autonomous task writes `tx_offset_hold_hz`
                            // directly with no operator keypress, so the
                            // TUI's own `App.tx_offset_hold_hz` copy would
                            // otherwise go stale. Echo it through so the
                            // park line + HOLD chip stay in sync.
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::TxOffsetUpdate {
                                    offset_hz,
                                },
                            );
                        }
                        MessageType::FoxModeStatus { on } => {
                            // Echo the authoritative Fox-mode state to the TUI
                            // FOX chip. Sent by the SetFoxMode handler on every
                            // path (engage, refused engage, disengage).
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::FoxModeUpdate { on },
                            );
                        }
                        MessageType::ActiveQsosSnapshot {
                            ref qsos,
                            ref pending,
                        } => {
                            // Re-shape into the TUI's ActiveQsoBanner
                            // (decoupled struct so the TUI doesn't link
                            // pancetta_qso). Push as a TuiMessage; the
                            // TUI replaces its previous list with this.
                            // Batch 94: carries the QSO-detail panel
                            // fields too (last TX/RX message, SNR,
                            // reports, exchange count).
                            // #40: pending cross-parity calls are carried in
                            // the same push as pending_calls so the TUI sees
                            // a consistent (active, queued) pair.
                            let banner_qsos: Vec<pancetta_tui::app::ActiveQsoBanner> =
                                qsos.iter().map(map_qso_snapshot_item).collect();
                            let banner_pending: Vec<pancetta_tui::app::PendingCallBanner> =
                                pending.iter().map(map_pending_call_item).collect();
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::ActiveQsosUpdate {
                                    qsos: banner_qsos,
                                    pending_calls: banner_pending,
                                },
                            );
                        }
                        MessageType::QsoHistoryEntry {
                            call_sign,
                            band,
                            success,
                            reason,
                            completed_at,
                        } => {
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::QsoHistoryEntry {
                                    call_sign,
                                    band,
                                    success,
                                    reason,
                                    completed_at,
                                },
                            );
                        }
                        MessageType::RigControl(
                            crate::message_bus::RigControlMessage::FrequencyResponse {
                                vfo,
                                frequency,
                            },
                        ) => {
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
                        MessageType::RigControl(
                            crate::message_bus::RigControlMessage::SwrResponse { swr },
                        ) => {
                            // Real rig SWR (hamlib SWR) sampled while keyed —
                            // forward verbatim; the TUI shows it only during TX.
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::SwrUpdate { swr },
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
                            let band_needed = dxcc_needed_on_band_for(
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
                                    needed,
                                    atno,
                                    band_needed,
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
                        MessageType::DiagnosticEvent {
                            target,
                            level,
                            ref text,
                            ref qso_id,
                            ref callsign,
                        } => {
                            // docs/observability-diagnostics-plan.md Layer 1 —
                            // relay the retained diagnostic stream into the TUI's
                            // bounded event history (Shift+D overlay).
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DiagnosticEvent {
                                    ts: chrono::Utc::now(),
                                    target,
                                    level,
                                    text: text.clone(),
                                    qso_id: qso_id.clone(),
                                    callsign: callsign.clone(),
                                },
                            );
                        }
                        MessageType::RecentQsoOutcome(ref outcome) => {
                            // docs/observability-diagnostics-plan.md Layer 2 —
                            // relay the structured terminal-QSO-outcome stream
                            // into the TUI's own bounded ring (App::recent_qsos),
                            // sibling to the DiagnosticEvent relay above.
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::RecentQsoOutcome(
                                    map_recent_qso_outcome(outcome),
                                ),
                            );
                        }
                        MessageType::TxPlacementUpdate { ref snapshot } => {
                            // TX-placement instrument (Task 9/10): convert the
                            // qso-crate PlacementSnapshot into the TUI-local
                            // PlacementView field-for-field — pancetta-tui must
                            // not depend on pancetta-qso, so this relay is the
                            // ONLY place the conversion happens.
                            let slices = snapshot
                                .slices
                                .iter()
                                .map(|c| pancetta_tui::app::PlacementSlice {
                                    offset_hz: c.offset_hz,
                                    score: c.score,
                                    clear_first: c.clear_first,
                                    clear_second: c.clear_second,
                                })
                                .collect();
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::TxPlacementUpdate {
                                    view: pancetta_tui::app::PlacementView {
                                        slices,
                                        openness: snapshot.openness.clone(),
                                        bin_hz: snapshot.bin_hz,
                                        range: snapshot.range,
                                        received_at: chrono::Utc::now(),
                                    },
                                },
                            );
                        }
                        MessageType::DxWatchlistUpdate { ref callsigns } => {
                            let _ = tui_msg_tx_relay.send(
                                pancetta_tui::tui_runner::TuiMessage::DxWatchlistUpdate {
                                    callsigns: callsigns.clone(),
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
                        last_decode_elapsed_ms: decode_last_elapsed_ms_relay
                            .load(Ordering::Relaxed),
                        last_decode_budget_exhausted: decode_last_budget_exhausted_relay
                            .load(Ordering::Relaxed),
                        tx_attempts: super::tx::tx_attempts_count(),
                        tx_defers: super::tx::tx_defers_count(),
                        decode_panic_count: super::ft8::decode_panic_count(),
                        wdt_panic_count: super::health::panic_count(),
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

                    // RX-input fallback badge — push only when it changes.
                    let input_fb = audio_input_fallback_relay.load(Ordering::Relaxed);
                    if last_input_fallback != Some(input_fb) {
                        last_input_fallback = Some(input_fb);
                        let _ = tui_msg_tx_relay.send(
                            pancetta_tui::tui_runner::TuiMessage::AudioInputFallback {
                                active: input_fb,
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
        let cmd_operating_freq_hz = self.operating_frequency_hz.clone();
        // Split TX dial atomic (0 = simplex). Written by SetSplit relay, read
        // by QSO RF-stamp and cleared here on any band change.
        let cmd_split_tx_hz = self.split_tx_frequency_hz();
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
        let cmd_tx_restart_inhibit = self.tx_restart_inhibit.clone();
        // PAN-19 round-12 review (Codex P1): "apply loop readiness to
        // direct PTT commands". `TogglePtt` below now routes its PTT-on
        // gate through the SAME `tx::tx_hard_mute_reason` helper the TX
        // worker uses (see that function's doc comment in
        // `coordinator/tx.rs`), which additionally checks this flag --
        // previously only `cmd_tx_restart_inhibit` was checked here, so a
        // manual PTT toggle during a slow (but non-fatal) Hamlib startup
        // could queue a key-up the loop consumed once it finally became
        // ready, well after the operator's actual keypress.
        let cmd_hamlib_command_loop_ready = self.hamlib_command_loop_ready.clone();
        // PAN-19 round-14 review (Codex P1): "keep TX muted until pending
        // rig state is delivered" -- see `tx_hard_mute_reason`'s doc
        // comment in `coordinator/tx.rs`.
        let cmd_hamlib_pending_frequency = self.hamlib_pending_frequency.clone();
        let cmd_hamlib_pending_split = self.hamlib_pending_split.clone();
        // PAN-19 round-16 review (Codex P1): "keep restored rig state
        // pending through CAT application" -- see `tx_hard_mute_reason`'s
        // doc comment in `coordinator/tx.rs`.
        let cmd_hamlib_command_in_flight = self.hamlib_command_in_flight.clone();
        // Operator Hold/Auto TX-frequency mode (`f`). The handler toggles this
        // atomic; the QSO engine and autonomous operator read it to gate
        // autonomous frequency moves.
        let cmd_tx_freq_mode = self.tx_freq_mode.clone();
        // PAN-39: bumped alongside every `cmd_tx_freq_mode.store()` below so
        // the autonomous operator can detect a transition it didn't directly
        // observe (see `AutonomousOperator::set_tx_freq_mode_generation_source`).
        let cmd_tx_freq_mode_generation = self.tx_freq_mode_generation.clone();
        // Held TX audio offset in Hz (0 = Auto/unset). Written by the `o`-modal
        // relay arm; read by the manual-call handler at QSO open to place our
        // TX audio offset when the operator has set one.
        let cmd_tx_offset_hold_hz = self.tx_offset_hold_hz();
        // decoder-speed-overhaul Task 15: live decode-effort cycling (`e`).
        // The handler cycles `cmd_current_decode_effort`, resolves `Auto`
        // against `cmd_resolved_hardware_tier`, and writes the resulting
        // budget into `cmd_decode_effort_budget_ms` — the same atomic
        // `ft8.rs` reads at both budgeted decode call sites.
        let cmd_current_decode_effort = self.current_decode_effort();
        let cmd_resolved_hardware_tier = self.resolved_hardware_tier();
        let cmd_decode_effort_budget_ms = self.decode_effort_budget_ms();
        // Mode-switch machinery (Shift+M). Cloned here (not previously
        // needed by this task) so the CycleOperatingMode handler can call
        // `try_switch_operating_mode` directly, mirroring how `cmd_tx_policy`
        // is used by CycleTxPolicy. `ft8_config`/`active_tx_qsos` are
        // `pub(crate)` fields so they clone directly with no accessor;
        // `active_protocol_mode`/`active_slot_ns`/`active_decode_phase_ns` go
        // through their `pub(crate) fn` accessors.
        let cmd_active_tx_qsos = self.active_tx_qsos.clone();
        // PAN-72: `u` "nudge" keystroke. `cmd_active_tx_offsets` supplies the
        // current offset (the `avoid_hz` to switch away from) for the
        // active-QSO branch; the other two feed the SAME mailbox/flag
        // Task 8/9 built (`pending_qso_offset_requests` is drained by the
        // Autonomous task via `apply_tx_offset_switch`, same as a stall-
        // detected switch; `pending_cq_offset_nudge` is drained into
        // `AutonomousOperator::request_manual_switch` for the CQ-hunting
        // fallback when no QSO is active) — this relay never holds a
        // `QsoManager` handle of its own, matching the established
        // AbortQso/ResendQso pattern of forwarding rather than mutating
        // QsoManager directly from this task.
        let cmd_active_tx_offsets = self.active_tx_offsets.clone();
        let cmd_pending_qso_offset_requests = self.pending_qso_offset_requests.clone();
        let cmd_pending_cq_offset_nudge = self.pending_cq_offset_nudge.clone();
        let cmd_ft8_config = self.ft8_config.clone();
        let cmd_active_protocol_mode = self.active_protocol_mode();
        let cmd_active_slot_ns = self.active_slot_ns();
        let cmd_active_decode_phase_ns = self.active_decode_phase_ns();
        // Remote-gateway relay gate (dispensa Q-0027 mode event) — the
        // CycleOperatingMode success arm relays the new mode to already-
        // connected remote clients, same additive pattern as Frequency/Split.
        let cmd_display_feed_enabled = self.display_feed_enabled.clone();
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
        // chosen output device into it (and into `cmd_config_path`).
        let cmd_config = self.config.clone();
        // PAN-62: the config file `main.rs` actually loaded from --
        // `--config <path>` if given, else the same default
        // `~/.pancetta/pancetta.toml`. SelectDevice, SelectRig,
        // SaveRigBookmark, and DeleteRigBookmark all persist here instead
        // of a hardcoded `~/.pancetta/pancetta.toml`, so an operator
        // running under `--config <custom-path>` doesn't have picker/
        // bookmark saves silently land in the wrong file.
        let cmd_config_path = self.config_path.clone();
        // PAN-62 review round 2 (Codex P1): precomputed at startup (never
        // in this relay loop, which must stay free of synchronous disk
        // I/O -- see `config_write_is_toml`'s doc comment in main.rs).
        let cmd_config_write_is_toml = self.config_write_is_toml;
        // PAN-61 review round 7 (Codex P1, superseding round 6's Mutex):
        // serializes every targeted-write persist call to
        // `cmd_config_path` -- SelectDevice, SelectRig, SaveRigBookmark,
        // and DeleteRigBookmark all write into it via `write_secure_atomic`,
        // which reuses the same `<path>.tmp` sibling for every caller. See
        // `spawn_config_file_write_worker`'s doc comment for why a plain
        // shared lock isn't enough here.
        let (cmd_config_write_tx, cmd_config_write_handle) = spawn_config_file_write_worker();
        // PAN-61 review round 8 (Codex P1): registered so graceful
        // shutdown (`shutdown.rs`) awaits this worker draining any writes
        // already queued, with the same bounded per-task timeout every
        // other task here gets, instead of the runtime silently
        // cancelling it mid-drain on teardown. Registration itself is
        // deferred -- see the round-9 fix below, right after `cmd_handle`
        // is pushed.
        //
        // Serializes the bookmark stage-mutate-commit sequence across the
        // two bookmark commands specifically -- see
        // `spawn_bookmark_mutation_worker`'s doc comment.
        let (cmd_bookmark_mutation_tx, cmd_bookmark_mutation_handle) =
            spawn_bookmark_mutation_worker(
                cmd_config.clone(),
                cmd_config_write_tx.clone(),
                cmd_tui_msg_tx.clone(),
                cmd_config_path.clone(),
                cmd_config_write_is_toml,
            );
        // Live device-switch channel into the audio thread. `None` in
        // stub/`--no-audio` modes — the SelectDevice handler then persists the
        // choice (applies on next restart) and tells the operator it can't apply
        // live in this mode.
        let cmd_audio_reopen_tx = self.audio_reopen_tx.clone();
        // Live rig-reconnect channel into run_main_loop (PAN-59). Always
        // present (see the field's doc comment in coordinator/mod.rs) --
        // unlike `cmd_audio_reopen_tx`, this is never `None`.
        let cmd_hamlib_reconnect_tx = self.hamlib_reconnect_tx.clone();
        // F4 toggle state: Some(t) when a tune is in flight and expected
        // to auto-stop at instant t. None when no tune is queued. The
        // coordinator owns this — TUI just emits ToggleTune events.
        let cmd_tune_until: std::sync::Arc<tokio::sync::RwLock<Option<tokio::time::Instant>>> =
            std::sync::Arc::new(tokio::sync::RwLock::new(None));
        // Fox-mode flag (shared with the QSO component). The ToggleFoxMode
        // relay arm reads the current value and sends `SetFoxMode { on: !current }`
        // so a single keypress always flips the authoritative state, even if the
        // TUI's optimistic local flip diverged (network lag, failed engage, etc.).
        let cmd_fox_mode = self.fox_mode();
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

            // Push the current rig config + enumerated serial ports to the
            // TUI once at startup (PAN-59), so the `i` rig-picker modal can
            // list them. Mirrors the DeviceListUpdate push immediately
            // above -- the coordinator enumerates hardware, the TUI stays a
            // passive renderer.
            {
                let (current_model, current_port, current_baud_rate, current_ptt_method, bookmarks) = {
                    let cfg = cmd_config.read().await;
                    (
                        cfg.rig.model.clone(),
                        cfg.rig.interface.port.clone(),
                        cfg.rig.interface.baud_rate,
                        cfg.rig.ptt.method.clone(),
                        cfg.rig.effective_bookmarks().to_vec(),
                    )
                };
                let available_ports = serialport::available_ports()
                    .map(|ports| ports.into_iter().map(|p| p.port_name).collect())
                    .unwrap_or_default();
                if let Err(e) =
                    cmd_tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::RigConfigUpdate {
                        available_ports,
                        current_model,
                        current_port,
                        current_baud_rate,
                        current_ptt_method,
                        bookmarks,
                    })
                {
                    debug!("Failed to send initial rig config to TUI: {}", e);
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

            // Seed the Fox-mode chip so it is authoritative from frame 1.
            // Mirrors the TxPolicyUpdate seed above: fox_mode defaults false
            // and the chip only corrects on SetFoxMode echo — push the real
            // atomic value once so a pre-configured fox_mode=true shows correctly.
            {
                let on = cmd_fox_mode.load(Ordering::Acquire);
                let _ =
                    cmd_tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::FoxModeUpdate { on });
            }

            // Seed the decode-effort chip so it is authoritative from frame 1
            // (decoder-speed-overhaul Task 15) — mirrors the TxPolicyUpdate /
            // FoxModeUpdate seeds above. Without this, a persisted
            // `tui_state.json` guess (or the field's zero-value default)
            // would show until the operator's first `e` press; pushing the
            // REAL seeded preset + budget once at startup means the chip is
            // never stale, even across a restart with a non-default
            // `[decoder].effort`.
            {
                let effort = pancetta_config::DecodeEffort::from_u8(
                    cmd_current_decode_effort.load(Ordering::Acquire),
                )
                .label()
                .to_string();
                let budget_ms = cmd_decode_effort_budget_ms.load(Ordering::Acquire);
                let _ =
                    cmd_tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::DecodeEffortUpdate {
                        effort,
                        budget_ms,
                    });
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
                                    origin: crate::message_bus::TxOrigin::Local,
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
                            // Refuse to call our own station — catches the case
                            // where our callsign slipped into the DX Hunter and
                            // the operator accidentally pressed Space on it.
                            if pancetta_qso::exchange::callsigns_match(&callsign, &cmd_our_callsign)
                            {
                                warn!(
                                    target: "qso.security",
                                    "Refusing to call our own station {}", callsign
                                );
                                crate::coordinator::tx::emit_diagnostic_full(
                                    &cmd_message_bus,
                                    ComponentId::Qso,
                                    "qso.security",
                                    pancetta_core::DiagnosticLevel::Warn,
                                    format!("Refusing to call our own station {callsign}"),
                                    None,
                                    Some(&callsign),
                                )
                                .await;
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "TX".to_string(),
                                        status: format!(
                                            "Refusing to call our own station ({})",
                                            callsign
                                        ),
                                    },
                                );
                                continue;
                            }
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
                                    // TUI-initiated call is LOCAL (byte-identical to prior).
                                    remote_origin: false,
                                }),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward CallStation command: {}", e);
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::EngageHound {
                            callsign,
                            fox_freq,
                            dx_parity,
                            fox_grid,
                        } => {
                            // EngageHound initiates a Hound QSO with the Fox.
                            // Gated by the TX policy exactly like CallStation —
                            // Hound engagement is an initiation (the Hound calls
                            // the Fox first), so RespondOnly/Disabled both refuse.
                            let policy = pancetta_core::TxPolicy::from_u8(
                                cmd_tx_policy.load(Ordering::Acquire),
                            );
                            // Refuse to call our own station in Hound mode.
                            if pancetta_qso::exchange::callsigns_match(&callsign, &cmd_our_callsign)
                            {
                                warn!(
                                    target: "qso.security",
                                    "Refusing EngageHound on our own callsign {}", callsign
                                );
                                crate::coordinator::tx::emit_diagnostic_full(
                                    &cmd_message_bus,
                                    ComponentId::Qso,
                                    "qso.security",
                                    pancetta_core::DiagnosticLevel::Warn,
                                    format!("Refusing EngageHound on our own callsign {callsign}"),
                                    None,
                                    Some(&callsign),
                                )
                                .await;
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "TX".to_string(),
                                        status: format!(
                                            "Refusing Hound on our own station ({})",
                                            callsign
                                        ),
                                    },
                                );
                                continue;
                            }
                            if !policy.allows_initiation() {
                                warn!(
                                    target: "tx.policy",
                                    "Refusing EngageHound {} ({} Hz): TX policy is {} \
                                     (initiation disallowed)",
                                    callsign, fox_freq, policy.label()
                                );
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "TX".to_string(),
                                        status: format!(
                                            "Can't engage Hound on {} — TX policy is {} \
                                             (press g for Full)",
                                            callsign,
                                            policy.label()
                                        ),
                                    },
                                );
                                continue;
                            }
                            info!(
                                "TUI EngageHound: Fox={} at {} Hz parity={:?} grid={:?}",
                                callsign, fox_freq, dx_parity, fox_grid
                            );
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                    component: "TX".to_string(),
                                    status: format!(
                                        "Hound: calling {} low @ {} Hz",
                                        callsign, fox_freq
                                    ),
                                },
                            );
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(
                                    crate::message_bus::QsoMessage::EngageHound {
                                        callsign,
                                        fox_freq,
                                        dx_parity,
                                        fox_grid,
                                    },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward EngageHound command: {}", e);
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
                                        // TUI-initiated answer is LOCAL.
                                        remote_origin: false,
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
                                // A band change invalidates any split TX freq.
                                if cmd_split_tx_hz.swap(0, Ordering::Relaxed) != 0 {
                                    // Push authoritative split clear to the TUI chip.
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::SplitUpdate {
                                            tx_hz: 0,
                                        },
                                    );
                                    let clr = ComponentMessage::new(
                                        ComponentId::Tui,
                                        ComponentId::Hamlib,
                                        MessageType::RigControl(
                                            crate::message_bus::RigControlMessage::SetSplit {
                                                enabled: false,
                                                tx_frequency: 0,
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    let _ = cmd_message_bus.send_message(clr).await;
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
                        pancetta_tui::tui_runner::TuiCommand::SetSplit {
                            enabled,
                            tx_frequency,
                        } => {
                            let store = if enabled { tx_frequency } else { 0 };
                            cmd_split_tx_hz.store(store, Ordering::Relaxed);
                            info!(target: "rig.split", "TUI SetSplit enabled={} tx={} Hz", enabled, tx_frequency);
                            // Push authoritative split state to the TUI chip.
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::SplitUpdate { tx_hz: store },
                            );
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Hamlib,
                                MessageType::RigControl(
                                    crate::message_bus::RigControlMessage::SetSplit {
                                        enabled,
                                        tx_frequency,
                                    },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward SetSplit to hamlib: {}", e);
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
                                    // TUI `c` key CQ is LOCAL.
                                    remote_origin: false,
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
                        pancetta_tui::tui_runner::TuiCommand::CycleOperatingMode => {
                            // Operator pressed Shift+M: cycle the station-wide
                            // operating mode FT8 → FT4 → FT2 → FT8. Unlike
                            // CycleTxPolicy this can be REFUSED (a QSO is
                            // active) — no optimistic local flip; the TUI
                            // waits for either a ModeUpdate (success) or a
                            // StatusUpdate (refusal) echo.
                            let current = pancetta_config::OperatingMode::from_u8(
                                cmd_active_protocol_mode.load(Ordering::Relaxed),
                            );
                            let next = current.cycle();
                            match super::try_switch_operating_mode(
                                next,
                                &cmd_active_tx_qsos,
                                &cmd_ft8_config,
                                &cmd_active_protocol_mode,
                                &cmd_active_slot_ns,
                                &cmd_active_decode_phase_ns,
                            ) {
                                Ok(()) => {
                                    let mode_str = super::mode_str(next).to_string();
                                    warn!(
                                        target: "operator.override",
                                        "Operator switched operating mode: {} -> {}",
                                        super::mode_str(current),
                                        mode_str
                                    );
                                    let set_mode_msg = ComponentMessage::new(
                                        ComponentId::Tui,
                                        ComponentId::Qso,
                                        MessageType::QsoMessage(
                                            crate::message_bus::QsoMessage::SetOperatingMode {
                                                mode: mode_str.clone(),
                                            },
                                        ),
                                        Instant::now(),
                                    );
                                    if let Err(e) = cmd_message_bus.send_message(set_mode_msg).await
                                    {
                                        warn!(
                                            "Failed to notify QSO component of mode switch: {}",
                                            e
                                        );
                                    }
                                    // dispensa Q-0027: relay the new mode to already-
                                    // connected remote-gateway clients, same additive
                                    // pattern as Frequency/Split (no-op when the
                                    // gateway is disabled).
                                    super::remote_gateway::relay_to_gateway(
                                        &cmd_message_bus,
                                        &cmd_display_feed_enabled,
                                        ComponentId::Tui,
                                        MessageType::ModeStatus {
                                            mode: mode_str.clone(),
                                        },
                                    )
                                    .await;
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::ModeUpdate {
                                            mode: mode_str,
                                        },
                                    );
                                    // FT2 is feature-gated in pancetta-ft8 behind `ft2`
                                    // (see `protocol_from_mode`). On a build without it,
                                    // switching TO "FT2" silently runs FT8 protocol
                                    // params/timing under the FT2 label — ADIF logs
                                    // MODE:FT2 while actually on-air FT8. Warn the
                                    // operator rather than switch silently; no-op on an
                                    // `ft2`-enabled build.
                                    #[cfg(not(feature = "ft2"))]
                                    if next == pancetta_config::OperatingMode::Ft2 {
                                        let _ = cmd_tui_msg_tx.send(
                                            pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                                component: "Mode".to_string(),
                                                status: "FT2 protocol is not yet implemented \
                                                    on this build — running FT8 timing under \
                                                    the FT2 label"
                                                    .to_string(),
                                            },
                                        );
                                    }
                                }
                                Err(super::ModeSwitchError::QsosActive(n)) => {
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "Mode".to_string(),
                                            status: format!(
                                                "can't switch mode: {} QSO(s) active",
                                                n
                                            ),
                                        },
                                    );
                                }
                                Err(super::ModeSwitchError::QsoSetUnavailable) => {
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "Mode".to_string(),
                                            status: "can't switch mode: QSO state unavailable"
                                                .to_string(),
                                        },
                                    );
                                }
                                Err(super::ModeSwitchError::ConfigLockBusy) => {
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "Mode".to_string(),
                                            status: "mode switch busy, try again".to_string(),
                                        },
                                    );
                                }
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::CycleDecodeEffort => {
                            // Operator pressed `e`: cycle the live decode-effort
                            // preset Eco → Standard → Deep → Max → Auto → Eco
                            // (decoder-speed-overhaul Task 15). Unlike
                            // CycleOperatingMode this can NEVER be refused — a
                            // budget change never invalidates in-flight decode
                            // state (spec §6.2) — so there is no active-QSO
                            // gate and no optimistic local flip on the TUI side;
                            // the TUI simply waits for this authoritative echo.
                            let tier = pancetta_ft8::tier_probe::HardwareTier::from_u8(
                                cmd_resolved_hardware_tier.load(Ordering::Acquire),
                            );
                            let (next, budget_ms) = super::effort::cycle_decode_effort(
                                &cmd_current_decode_effort,
                                &cmd_decode_effort_budget_ms,
                                tier,
                            );
                            let label = next.label().to_string();
                            info!(
                                target: "decoder.effort",
                                "Operator cycled decode effort -> {} (budget={}ms, tier={})",
                                label,
                                budget_ms,
                                tier.as_str()
                            );
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::DecodeEffortUpdate {
                                    effort: label,
                                    budget_ms,
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
                            // PAN-38 round 3 (Codex): bump the generation
                            // BEFORE storing the new mode, both SeqCst,
                            // paired with `AutonomousOperator::tx_freq_auto`/
                            // the PAN-39 generation check reading in
                            // "mode first, generation second" order --
                            // see those call sites' doc comments for why
                            // this specific pairing (and not just Acquire/
                            // Release on each independently) closes the
                            // race where a concurrent reader could observe
                            // the new mode but the pre-bump generation,
                            // firing neither invalidation check.
                            cmd_tx_freq_mode_generation.fetch_add(1, Ordering::SeqCst);
                            cmd_tx_freq_mode.store(next.as_u8(), Ordering::SeqCst);
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
                        pancetta_tui::tui_runner::TuiCommand::SetTxOffset { offset_hz } => {
                            // Operator used the `o` modal to set or clear the held
                            // TX audio offset.
                            // Some(hz) → store hz, flip mode to Hold so the offset
                            //            is actually used by the manual-call handler.
                            // None     → store 0 (Auto/unset), flip mode to Auto.
                            match offset_hz {
                                Some(hz) => {
                                    cmd_tx_offset_hold_hz.store(hz, Ordering::Relaxed);
                                    // PAN-38 round 3: generation-before-mode,
                                    // both SeqCst -- see ToggleTxFreqMode's
                                    // comment above for the full reasoning.
                                    cmd_tx_freq_mode_generation.fetch_add(1, Ordering::SeqCst);
                                    cmd_tx_freq_mode.store(
                                        pancetta_core::TxFreqMode::Hold.as_u8(),
                                        Ordering::SeqCst,
                                    );
                                    info!(
                                        target: "tx.freq",
                                        "Operator set TX offset hold @ {} Hz (mode → Hold)",
                                        hz
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: format!("TX offset held @ {} Hz", hz),
                                        },
                                    );
                                }
                                None => {
                                    cmd_tx_offset_hold_hz.store(0, Ordering::Relaxed);
                                    // PAN-38 round 3: generation-before-mode,
                                    // both SeqCst -- see ToggleTxFreqMode's
                                    // comment above for the full reasoning.
                                    cmd_tx_freq_mode_generation.fetch_add(1, Ordering::SeqCst);
                                    cmd_tx_freq_mode.store(
                                        pancetta_core::TxFreqMode::Auto.as_u8(),
                                        Ordering::SeqCst,
                                    );
                                    info!(
                                        target: "tx.freq",
                                        "Operator cleared TX offset hold (mode → Auto)"
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: "TX offset auto (Tx=Rx)".to_string(),
                                        },
                                    );
                                }
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::NudgeTxOffset => {
                            // PAN-72: prefer an active QSO (force a Switch,
                            // bypassing stall_cycles -- operator-forced);
                            // fall back to the CQ-hunting one-shot flag if
                            // none is active. Does not touch
                            // tx_freq_mode/tx_offset_hold_hz -- this must
                            // not leave Auto mode. Actual decision logic
                            // lives in `resolve_nudge_tx_offset` (directly
                            // unit-tested); this arm just snapshots
                            // `active_tx_qsos` and echoes a status line.
                            let active: std::collections::HashSet<String> = cmd_active_tx_qsos
                                .read()
                                .map(|s| s.clone())
                                .unwrap_or_default();
                            let switched = resolve_nudge_tx_offset(
                                &active,
                                &cmd_active_tx_offsets,
                                &cmd_pending_qso_offset_requests,
                                &cmd_pending_cq_offset_nudge,
                            );
                            match switched {
                                Some(qso_id) => {
                                    info!(
                                        target: "tx.freq",
                                        "TUI NudgeTxOffset: forcing offset switch for active QSO {}",
                                        qso_id
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: "Nudging active QSO to a new offset"
                                                .to_string(),
                                        },
                                    );
                                }
                                None => {
                                    info!(
                                        target: "tx.freq",
                                        "TUI NudgeTxOffset: no active QSO, requesting a CQ-offset nudge"
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: "Nudging CQ offset (once hunting)".to_string(),
                                        },
                                    );
                                }
                            }
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
                                if !policy.allows_any_tx()
                                    || cmd_tx_restart_inhibit.load(Ordering::Acquire) != 0
                                {
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
                                // PAN-19 round-12 review (Codex P1): route
                                // through `ptt_on_refusal`, which itself
                                // routes through the SAME shared gate the
                                // TX worker uses (`tx::tx_hard_mute_reason`)
                                // -- see both functions' doc comments for
                                // why. This additionally checks
                                // `hamlib_command_loop_ready`, closing the
                                // coverage gap where a manual PTT toggle
                                // during a slow-but-non-fatal Hamlib
                                // startup could queue a key-up the loop
                                // only consumed once it became ready,
                                // unexpectedly keying the radio well after
                                // the operator's actual keypress.
                                if let Some(status) = ptt_on_refusal(
                                    &cmd_tx_policy,
                                    &cmd_tx_restart_inhibit,
                                    &cmd_hamlib_command_loop_ready,
                                    &cmd_hamlib_pending_frequency,
                                    &cmd_hamlib_pending_split,
                                    &cmd_hamlib_command_in_flight,
                                ) {
                                    warn!(
                                        target: "tx.policy",
                                        "Refusing PTT key-up: {status}"
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status,
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
                            // config and to `cmd_config_path` so it survives
                            // a restart, AND apply it live by asking the
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
                            // PAN-62: the actually-loaded config path
                            // (`--config <path>`, or the default), not a
                            // hardcoded `~/.pancetta/pancetta.toml`.
                            let config_path = cmd_config_path.clone();
                            // PAN-61 review round 7 (P1): submit to the
                            // serialized write worker and do NOT await the
                            // result inline -- the persist outcome is
                            // reported from a short spawned task instead
                            // (mirrors SelectRig's existing reconnect-wait
                            // pattern below), so this arm never blocks the
                            // relay loop on disk I/O, no matter how long
                            // this write or anything queued ahead of it
                            // takes. The live-switch dance a few lines down
                            // is unconditional either way (not gated on
                            // persist outcome), exactly as before.
                            // PAN-62 review round 1 (Codex P2), format
                            // precomputed at startup per round 2 (Codex
                            // P1): the targeted setters below parse the
                            // existing file exclusively as TOML --
                            // persisting against a JSON `--config` file
                            // would fail every time with a raw parser
                            // error. Reject up front with a clear
                            // operator-facing message instead.
                            if !cmd_config_write_is_toml {
                                warn!(
                                    "Config file {} is not TOML; audio device selection not persisted",
                                    config_path.display()
                                );
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "audio".to_string(),
                                        status: "Device choice applied live, but not persisted \
                                                 (config file isn't TOML)"
                                            .to_string(),
                                    },
                                );
                            } else {
                                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                let write_path = config_path.clone();
                                let write_input = input_device.clone();
                                let write_output = output_device.clone();
                                let submitted = cmd_config_write_tx.send(ConfigFileWriteRequest {
                                    work: Box::new(move || {
                                        pancetta_config::Config::default()
                                            .set_audio_devices_in_file(
                                                &write_path,
                                                write_input.as_deref(),
                                                write_output.as_deref(),
                                            )
                                    }),
                                    respond: resp_tx,
                                });
                                if submitted.is_ok() {
                                    let report_tui_msg_tx = cmd_tui_msg_tx.clone();
                                    let report_path = config_path.clone();
                                    tokio::spawn(async move {
                                        match resp_rx.await {
                                            Ok(Ok(())) => {
                                                info!(
                                                    "Persisted audio device selection to {}",
                                                    report_path.display()
                                                );
                                            }
                                            Ok(Err(e)) => {
                                                warn!(
                                                    "Failed to persist audio device selection: {}",
                                                    e
                                                );
                                                let _ = report_tui_msg_tx.send(
                                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                                        component: "audio".to_string(),
                                                        status: format!(
                                                            "Failed to save device choice: {}",
                                                            e
                                                        ),
                                                    },
                                                );
                                            }
                                            Err(_) => {
                                                warn!(
                                                    "Config write worker dropped the response for audio device persist"
                                                );
                                            }
                                        }
                                    });
                                } else {
                                    warn!(
                                        "Config write worker channel closed; audio device selection not persisted"
                                    );
                                }
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
                        pancetta_tui::tui_runner::TuiCommand::SelectRig {
                            model,
                            port,
                            baud_rate,
                            ptt_method,
                        } => {
                            info!(
                                "TUI SelectRig: model={} port={} baud={} ptt={:?}",
                                model, port, baud_rate, ptt_method
                            );
                            // I-1a fix (PAN-59 final review): validate the
                            // model BEFORE persisting/reconnecting anything.
                            // Without this, an unrecognized model was
                            // written straight into the live config and
                            // ~/.pancetta/pancetta.toml, then
                            // `start_hamlib_component` would fall through
                            // to build a `RigctldClient` pointing at a port
                            // nothing is listening on -- reporting success
                            // for a config that can never work, and the bad
                            // value would survive a restart too.
                            if !crate::coordinator::hamlib::model_recognized(&model) {
                                warn!(
                                    "TUI SelectRig: unknown rig model '{}' -- not applied",
                                    model
                                );
                                let _ = cmd_tui_msg_tx.send(
                                    pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                        component: "rig".to_string(),
                                        status: format!(
                                            "Unknown rig model '{model}' — not applied"
                                        ),
                                    },
                                );
                            } else {
                                // Persist the operator's choice to the in-memory
                                // config and to `cmd_config_path` so it survives
                                // a restart, AND apply it live by asking
                                // run_main_loop (via hamlib_reconnect_tx) to tear
                                // down and reconnect Hamlib on the new config --
                                // same live-switch pattern as SelectDevice above.
                                {
                                    let mut cfg = cmd_config.write().await;
                                    cfg.rig.model = model.clone();
                                    cfg.rig.interface.port = port.clone();
                                    cfg.rig.interface.baud_rate = baud_rate;
                                    cfg.rig.ptt.method = ptt_method.clone();
                                }
                                // PAN-62: the actually-loaded config path
                                // (`--config <path>`, or the default), not
                                // a hardcoded `~/.pancetta/pancetta.toml`.
                                let config_path = cmd_config_path.clone();
                                // PAN-61 review round 7 (P1): submit to the
                                // serialized write worker and do NOT await
                                // the result inline -- see SelectDevice's
                                // identical comment above. The Hamlib
                                // reconnect request below is unconditional
                                // either way (not gated on persist
                                // outcome), exactly as before.
                                // PAN-62 review round 1 (Codex P2), format
                                // precomputed at startup per round 2
                                // (Codex P1): see SelectDevice's identical
                                // guard above.
                                if !cmd_config_write_is_toml {
                                    warn!(
                                        "Config file {} is not TOML; rig config selection not persisted",
                                        config_path.display()
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "rig".to_string(),
                                            status: "Rig applied live, but not persisted (config \
                                                     file isn't TOML)"
                                                .to_string(),
                                        },
                                    );
                                } else {
                                    let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                    let write_path = config_path.clone();
                                    let write_model = model.clone();
                                    let write_port = port.clone();
                                    let write_ptt = ptt_method.clone();
                                    let submitted =
                                        cmd_config_write_tx.send(ConfigFileWriteRequest {
                                            work: Box::new(move || {
                                                pancetta_config::Config::default().set_rig_in_file(
                                                    &write_path,
                                                    &write_model,
                                                    &write_port,
                                                    baud_rate,
                                                    write_ptt,
                                                )
                                            }),
                                            respond: resp_tx,
                                        });
                                    if submitted.is_ok() {
                                        let report_path = config_path.clone();
                                        tokio::spawn(async move {
                                            match resp_rx.await {
                                                Ok(Ok(())) => {
                                                    info!(
                                                        "Persisted rig config selection to {}",
                                                        report_path.display()
                                                    );
                                                }
                                                Ok(Err(e)) => {
                                                    warn!(
                                                        "Failed to persist rig config selection: {}",
                                                        e
                                                    );
                                                }
                                                Err(_) => {
                                                    warn!(
                                                        "Config write worker dropped the response for rig config persist"
                                                    );
                                                }
                                            }
                                        });
                                    } else {
                                        warn!(
                                            "Config write worker channel closed; rig config selection not persisted"
                                        );
                                    }
                                }

                                // I4 fix (PAN-59 review): this relay task's loop
                                // also services OperatorEmergencyStop/StopTx/
                                // TogglePtt/AbortQso -- an unbounded
                                // `.send().await` on the capacity-1
                                // `hamlib_reconnect_tx` channel (blocking until
                                // run_main_loop drains it) and then awaiting the
                                // full reconnect response INLINE would
                                // head-of-line-block every one of those safety
                                // commands behind an in-flight rig switch. Use
                                // `try_send` (bounded: fails immediately, never
                                // blocks) so a reconnect already in flight is
                                // reported instantly instead of stalling this
                                // loop, and hand the response wait off to a
                                // short-lived spawned task so the loop itself
                                // returns to `try_recv` immediately after handing
                                // off the request.
                                let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
                                match cmd_hamlib_reconnect_tx.try_send(
                                    crate::coordinator::hamlib::HamlibReconnectRequest {
                                        respond: resp_tx,
                                    },
                                ) {
                                    Ok(()) => {
                                        let report_model = model.clone();
                                        let report_tui_msg_tx = cmd_tui_msg_tx.clone();
                                        tokio::spawn(async move {
                                            // Bound the wait comfortably beyond
                                            // `teardown_hamlib`'s documented
                                            // worst case (~10s of PTT-off
                                            // retries) plus margin, so a slow-
                                            // but-genuinely-in-progress reconnect
                                            // isn't misreported as timed out.
                                            let status = match tokio::time::timeout(
                                                Duration::from_secs(20),
                                                resp_rx,
                                            )
                                            .await
                                            {
                                                Ok(Ok(Ok(()))) => {
                                                    info!(
                                                        "Live rig reconnect succeeded: {}",
                                                        report_model
                                                    );
                                                    format!("Rig → {} (live)", report_model)
                                                }
                                                Ok(Ok(Err(err))) => {
                                                    warn!("Live rig reconnect failed: {}", err);
                                                    format!(
                                                    "Rig config saved ({}) but reconnect failed: {} — kept previous connection",
                                                    report_model, err
                                                )
                                                }
                                                Ok(Err(_)) => {
                                                    warn!(
                                                    "run_main_loop dropped the reconnect response"
                                                );
                                                    format!(
                                                    "Rig config saved ({}) — no response from main loop; restart to apply",
                                                    report_model
                                                )
                                                }
                                                Err(_) => {
                                                    warn!("Timed out waiting for rig reconnect");
                                                    format!(
                                                    "Rig config saved ({}) — reconnect timed out; check rig connection",
                                                    report_model
                                                )
                                                }
                                            };
                                            let _ = report_tui_msg_tx.send(
                                            pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                                component: "rig".to_string(),
                                                status,
                                            },
                                        );
                                        });
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                                        warn!(
                                        "Rig switch already in progress; ignoring new SelectRig request"
                                    );
                                        let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "rig".to_string(),
                                            status: format!(
                                                "Rig config saved ({}) — a rig switch is already in progress; try again shortly",
                                                model
                                            ),
                                        },
                                    );
                                    }
                                    Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => {
                                        warn!(
                                        "Hamlib reconnect channel closed; rig config saved but not applied live"
                                    );
                                        let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "rig".to_string(),
                                            status: format!(
                                                "Rig config saved ({}) — live switch unavailable; restart to apply",
                                                model
                                            ),
                                        },
                                    );
                                    }
                                }
                            }
                        }
                        pancetta_tui::tui_runner::TuiCommand::SaveRigBookmark {
                            name,
                            model,
                            port,
                            baud_rate,
                            ptt_method,
                        } => {
                            info!(
                                "TUI SaveRigBookmark: name={} model={} port={} baud={} ptt={:?}",
                                name, model, port, baud_rate, ptt_method
                            );
                            // PAN-61 review round 7 (P1): hand off to the
                            // dedicated, serialized bookmark-mutation
                            // worker (see `spawn_bookmark_mutation_worker`'s
                            // doc comment) -- this `send` never awaits, so
                            // the relay loop returns to `try_recv`
                            // immediately no matter how long the worker's
                            // queue takes to drain.
                            let bookmark = pancetta_config::rig::RigBookmark {
                                name,
                                model,
                                port,
                                baud_rate,
                                ptt_method,
                            };
                            let _ = cmd_bookmark_mutation_tx.send(BookmarkMutation::Save(bookmark));
                        }
                        pancetta_tui::tui_runner::TuiCommand::DeleteRigBookmark { name } => {
                            info!("TUI DeleteRigBookmark: name={}", name);
                            let _ = cmd_bookmark_mutation_tx.send(BookmarkMutation::Delete(name));
                        }
                        pancetta_tui::tui_runner::TuiCommand::ToggleFoxMode => {
                            // Operator pressed `Shift+X`: toggle Fox mode.
                            // Read the authoritative atomic (not the TUI's
                            // optimistic local flag) and flip it via
                            // SetFoxMode so even a diverged TUI stays in sync.
                            // The SetFoxMode handler in the QSO component is the
                            // authority: it gates on TX policy (engage = initiation
                            // → refuse under RespondOnly/Disabled), starts/stops
                            // the repeating CQ, and raises/restores the
                            // caller-answer cap. We just forward the toggle.
                            let currently_on =
                                cmd_fox_mode.load(std::sync::atomic::Ordering::Acquire);
                            let on = !currently_on;
                            info!(
                                target: "operator.override",
                                "Operator toggled Fox mode: {} -> {}",
                                currently_on,
                                on
                            );
                            let msg = ComponentMessage::new(
                                ComponentId::Tui,
                                ComponentId::Qso,
                                MessageType::QsoMessage(
                                    crate::message_bus::QsoMessage::SetFoxMode { on },
                                ),
                                Instant::now(),
                            );
                            if let Err(e) = cmd_message_bus.send_message(msg).await {
                                warn!("Failed to forward ToggleFoxMode command: {}", e);
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
            // PAN-61 review round 10 (Codex P1): `tui_wrapper` (below, near
            // the TUI's spawn_blocking site) stores `shutdown_signal = true`
            // the instant the TUI's own event loop returns -- independent
            // of whether this relay loop has dequeued everything the TUI
            // already sent into `tui_cmd_rx`. An operator who saves or
            // deletes a bookmark and then quits in the same keystroke burst
            // can have that command still queued when the `while
            // !cmd_shutdown` guard above observes shutdown=true and exits.
            // Drain any remaining bookmark commands here, once, before this
            // task returns: `cmd_bookmark_mutation_tx` is still owned by
            // this closure and its worker (registered right after this
            // task, below) hasn't been touched by shutdown yet, so a late
            // `send` here still reaches it before the sender is dropped.
            while let Ok(cmd) = tui_cmd_rx.try_recv() {
                match cmd {
                    pancetta_tui::tui_runner::TuiCommand::SaveRigBookmark {
                        name,
                        model,
                        port,
                        baud_rate,
                        ptt_method,
                    } => {
                        info!(
                            "TUI SaveRigBookmark (post-shutdown drain): name={} model={} port={} baud={} ptt={:?}",
                            name, model, port, baud_rate, ptt_method
                        );
                        let bookmark = pancetta_config::rig::RigBookmark {
                            name,
                            model,
                            port,
                            baud_rate,
                            ptt_method,
                        };
                        let _ = cmd_bookmark_mutation_tx.send(BookmarkMutation::Save(bookmark));
                    }
                    pancetta_tui::tui_runner::TuiCommand::DeleteRigBookmark { name } => {
                        info!("TUI DeleteRigBookmark (post-shutdown drain): name={}", name);
                        let _ = cmd_bookmark_mutation_tx.send(BookmarkMutation::Delete(name));
                    }
                    _ => {}
                }
            }
            Ok(())
        });
        self.named_task_handles.push((ComponentId::Tui, cmd_handle));
        // PAN-61 review round 9 (Codex P1): register the two workers'
        // handles AFTER cmd_handle (the relay loop above), not before.
        // `shutdown.rs` drains `named_task_handles` in insertion order
        // with a bounded 1s-per-task timeout, aborting whatever hasn't
        // finished. `cmd_handle` is the task holding
        // `cmd_config_write_tx`/`cmd_bookmark_mutation_tx` -- the two
        // workers' recv loops only exit once every sender is dropped. If
        // the workers were drained first (as they were through round 8),
        // a relay loop still parked in an in-flight await (e.g. the
        // audio-reopen wait) can't drop its senders in time, so the
        // workers hit their own timeout and get aborted with queued
        // saves/deletes still unprocessed. Registering the producer
        // first lets shutdown close it -- dropping the senders -- before
        // the consumers are awaited, so the workers see channel closure
        // and drain cleanly within their own timeout.
        self.named_task_handles
            .push((ComponentId::Tui, cmd_config_write_handle));
        self.named_task_handles
            .push((ComponentId::Tui, cmd_bookmark_mutation_handle));

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
                mode: tui_config_lock.rig.mode.clone(),
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
            band_plan: tui_config_lock.rig.frequency.band_plan.clone(),
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
/// Map a bus `PendingCallSnapshotItem` to the TUI's `PendingCallBanner`.
fn map_pending_call_item(
    p: &crate::message_bus::PendingCallSnapshotItem,
) -> pancetta_tui::app::PendingCallBanner {
    pancetta_tui::app::PendingCallBanner {
        callsign: p.callsign.clone(),
        dx_parity: p.dx_parity,
        waited_secs: p.waited_secs,
    }
}

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
        dx_last_activity: q.dx_last_activity.clone(),
        hound: q.hound,
    }
}

/// Converts the bus's `RecentQsoOutcome` (docs/observability-diagnostics-
/// plan.md Layer 2) into the TUI-local mirror type
/// (`pancetta_tui::app::RecentQsoOutcome`) — `pancetta-tui` must not
/// depend on the `pancetta` binary crate, so this relay is the only place
/// the conversion happens (same reasoning as `map_qso_snapshot_item`
/// above). `QsoFailureReason` is reused unconverted since `pancetta-tui`
/// already depends on `pancetta-qso` directly.
fn map_recent_qso_outcome(
    outcome: &crate::message_bus::RecentQsoOutcome,
) -> pancetta_tui::app::RecentQsoOutcome {
    pancetta_tui::app::RecentQsoOutcome {
        callsign: outcome.callsign.clone(),
        outcome: match &outcome.outcome {
            crate::message_bus::QsoOutcome::Completed => pancetta_tui::app::QsoOutcome::Completed,
            crate::message_bus::QsoOutcome::Failed(reason) => {
                pancetta_tui::app::QsoOutcome::Failed(reason.clone())
            }
        },
        last_state: outcome.last_state.clone(),
        freq_hz: outcome.freq_hz,
        ts: outcome.ts,
        brief_timeline: outcome.brief_timeline.clone(),
    }
}

/// PAN-72: resolves and applies one `TuiCommand::NudgeTxOffset` dispatch.
/// Prefers forcing a `Switch` for whatever QSO is in `active` (feeding the
/// SAME `pending_qso_offset_requests` mailbox Task 8/9 built — this is NOT
/// a new commit path, and this relay task never holds a `QsoManager` handle
/// of its own; the Autonomous task's already-restart-safe `qso_manager_watch`
/// is what eventually calls `apply_tx_offset_switch`, same as a stall-
/// detected switch). Falls back to setting `pending_cq_offset_nudge` — a
/// one-shot flag the Autonomous task forwards into
/// `AutonomousOperator::request_manual_switch` — when no QSO is active.
///
/// Extracted to a plain, synchronously-testable function (mirroring
/// `tx_qso_is_live`/`should_repark`'s extraction out of the giant spawned
/// relay/tick-loop task) rather than only being exercised end-to-end through
/// a live `start_tui_pipeline` task — no such harness exists in this test
/// module for any existing `TuiCommand` arm (`SetTxOffset`/`ToggleAutonomous`
/// included), so this keeps the new match arm's actual decision logic
/// directly unit-testable.
///
/// `active_tx_qsos` keys are built via `active_tx_qso_key(&qso_id.to_string())`
/// (`coordinator/mod.rs`) -- `qso_id.to_string()` on a `QsoId` (`= Uuid`)
/// followed by `.trim().to_uppercase()`. `Uuid`'s `FromStr`/`parse_str` are
/// case-insensitive, so `key.parse::<pancetta_qso::QsoId>()` round-trips
/// this correctly with no separate inverse helper needed -- confirmed by
/// reading `active_tx_qso_key`'s real definition and its one call site
/// (`coordinator/qso.rs`), not assumed.
///
/// Returns `Some(qso_id)` when the active-QSO branch fired (for the
/// caller's status echo/logging), `None` for the CQ-hunting fallback.
fn resolve_nudge_tx_offset(
    active: &std::collections::HashSet<String>,
    active_tx_offsets: &std::sync::RwLock<std::collections::HashMap<String, f64>>,
    pending_qso_offset_requests: &std::sync::Mutex<
        Vec<(
            pancetta_qso::states::QsoId,
            pancetta_qso::qso_manager::OffsetAction,
        )>,
    >,
    pending_cq_offset_nudge: &std::sync::atomic::AtomicBool,
) -> Option<pancetta_qso::states::QsoId> {
    if let Some(key) = active.iter().next() {
        if let Ok(qso_id) = key.parse::<pancetta_qso::QsoId>() {
            let current = active_tx_offsets
                .read()
                .ok()
                .and_then(|m| m.get(key).copied())
                .unwrap_or(1500.0);
            if let Ok(mut pending) = pending_qso_offset_requests.lock() {
                pending.push((
                    qso_id,
                    pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz: current },
                ));
            }
            return Some(qso_id);
        }
    }
    pending_cq_offset_nudge.store(true, Ordering::Relaxed);
    None
}

/// The dial frequency (MHz) to stamp on this decode's `DecodedMessageView`.
///
/// PAN-67: this MUST come from the frequency the decode's own audio window
/// was captured on (`RelayedDecode::dial_hz`), never from a live re-read of
/// wherever the rig is tuned right now — decoding is real CPU work, and an
/// in-flight decode's audio can predate a since-happened band switch. The
/// signature enforces this at the type level: there is no live-state
/// parameter to read here even by accident.
///
/// `dial_hz == 0` means no rig / no CAT read has landed yet (see
/// `dsp.rs`'s `band_ref_dial_hz`/`cur_dial_hz`) — falls back to the same
/// 14.074 MHz default the removed live-atomic seed used, so a headless or
/// pre-first-read decode still enriches against a real band instead of
/// stamping 0.0 MHz and feeding 0 Hz into the worked/needed lookups
/// (PAN-67 review round 1 finding 2).
fn decode_view_dial_mhz(relayed: &super::pipeline::RelayedDecode) -> f64 {
    if relayed.dial_hz == 0 {
        14.074
    } else {
        relayed.dial_hz as f64 / 1_000_000.0
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

/// Is this callsign's DXCC entity needed specifically on the band implied
/// by `freq_hz` — i.e. never worked THERE before, per the local QSO
/// database — independent of `needed_atno_for` (which reflects cqdx's
/// needed set for the operator's currently-tuned band, not necessarily
/// this row's own band). See `WorkedStationLookup::is_dxcc_needed_on_band`
/// (2026-07-18, DX Hunter per-band-needed gap).
fn dxcc_needed_on_band_for(
    lookup: &crate::priority_evaluator::CachedStationLookup,
    callsign: Option<&str>,
    freq_hz: f64,
) -> bool {
    use pancetta_qso::priority::WorkedStationLookup;
    match callsign {
        Some(c) if !c.is_empty() => lookup.is_dxcc_needed_on_band(c, freq_hz),
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

/// PAN-19 round-12 review (Codex P1): "apply loop readiness to direct PTT
/// commands". The `TogglePtt` handler's PTT-on gate, factored out into its
/// own free function so it's directly unit-testable (the handler itself
/// lives deep inside a giant `tokio::select!` command loop with no
/// injectable seams). `None` means the key-up may proceed; `Some(status)`
/// is the operator-facing status text explaining the refusal.
///
/// This is a thin wrapper over `tx::tx_hard_mute_reason` -- THE single
/// shared gate every PTT-on call site in the coordinator must route
/// through (see that function's doc comment). Before this fix, `TogglePtt`
/// re-derived its own `tx_restart_inhibit`-only condition here instead of
/// calling it, which silently omitted the `hamlib_command_loop_ready`
/// check the TX worker had had since round 7 -- a manual PTT toggle during
/// a slow-but-non-fatal Hamlib startup could queue a key-up the loop only
/// consumed once it became ready. Routing through the shared function
/// (rather than duplicating its condition here) means a THIRD PTT-on call
/// site added later gets this gate for free instead of needing its own
/// copy that could omit a check again.
///
/// PAN-19 round-14 review (Codex P1): "keep TX muted until pending rig
/// state is delivered". `tx_hard_mute_reason` now also checks
/// `hamlib_pending_frequency`/`hamlib_pending_split` -- a manual PTT
/// toggle must be refused just as much as an automated TX-worker key-up
/// while a prior generation's SetFrequency/SetSplit is still waiting,
/// undelivered, for the polling task's retry (see that function's doc
/// comment).
///
/// PAN-19 round-16 review (Codex P1): "keep restored rig state pending
/// through CAT application". `tx_hard_mute_reason` now ALSO checks
/// `hamlib_command_in_flight` -- a manual PTT toggle must be refused while
/// the message loop is actively awaiting a `set_frequency`/
/// `set_split_freq`/`set_split` CAT call, not just while a pending slot is
/// populated (see that function's doc comment).
fn ptt_on_refusal(
    tx_policy: &Arc<std::sync::atomic::AtomicU8>,
    tx_restart_inhibit: &Arc<std::sync::atomic::AtomicU32>,
    hamlib_command_loop_ready: &Arc<std::sync::atomic::AtomicBool>,
    hamlib_pending_frequency: &Arc<
        std::sync::Mutex<std::collections::HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    hamlib_pending_split: &Arc<std::sync::Mutex<Option<ComponentMessage>>>,
    hamlib_command_in_flight: &Arc<std::sync::atomic::AtomicU32>,
) -> Option<String> {
    super::tx::tx_hard_mute_reason(
        tx_policy,
        tx_restart_inhibit,
        hamlib_command_loop_ready,
        hamlib_pending_frequency,
        hamlib_pending_split,
        hamlib_command_in_flight,
    )
    .map(|reason| format!("Can't key PTT — {reason}"))
}

/// Upsert a bookmark into a list by name (PAN-61: save-as, overwrite if the
/// name already exists). Extracted as a pure function so the semantics are
/// unit-testable outside the coordinator's spawned command-relay task --
/// mirrors the `ptt_on_refusal`/`map_qso_snapshot_item` pure-helper pattern
/// already used in this file's test module.
fn upsert_rig_bookmark(
    bookmarks: &mut Vec<pancetta_config::rig::RigBookmark>,
    bookmark: pancetta_config::rig::RigBookmark,
) {
    if let Some(existing) = bookmarks.iter_mut().find(|b| b.name == bookmark.name) {
        *existing = bookmark;
    } else {
        bookmarks.push(bookmark);
    }
}

/// Build the operator-facing status message after a bookmark save
/// (PAN-61). The 20-bookmark soft cap is advisory only: `count` past the
/// cap never blocks the save, it just appends a warning.
fn rig_bookmark_saved_status(name: &str, count: usize) -> String {
    if count >= 20 {
        format!(
            "Saved bookmark '{}' ({} bookmarks saved — consider deleting unused ones)",
            name, count
        )
    } else {
        format!("Saved bookmark '{}'", name)
    }
}

/// One targeted config-file write (PAN-62: the coordinator's `config_path`,
/// not always `~/.pancetta/pancetta.toml`), queued for serialized
/// processing by the worker `spawn_config_file_write_worker` spawns
/// (PAN-61 review round 7, P1). `work` performs the actual blocking
/// `std::fs` I/O (matching the `set_*_in_file` methods' shape) and is only
/// ever run on tokio's blocking-thread pool inside the worker, never
/// inline on the caller's own task.
struct ConfigFileWriteRequest {
    work: Box<dyn FnOnce() -> pancetta_config::ConfigResult<()> + Send + 'static>,
    respond: tokio::sync::oneshot::Sender<pancetta_config::ConfigResult<()>>,
}

/// Spawn the single serialized worker that processes every targeted
/// `pancetta.toml` write in this coordinator (`SelectDevice`, `SelectRig`,
/// `SaveRigBookmark`, `DeleteRigBookmark`) one at a time, and return the
/// sender callers submit requests through.
///
/// PAN-61 review round 6 tried a shared `Mutex` instead; round 7 (P1)
/// found that still suspended whichever task's `.lock().await` lost the
/// race -- when that task was the relay loop's own inline `SelectRig`/
/// `SelectDevice` arm, a concurrent bookmark write's disk I/O could delay
/// `OperatorEmergencyStop`/`StopTx`/`TogglePtt`/`AbortQso` all over again,
/// reopening exactly what rounds 4-5 fixed. Submitting a request to this
/// worker's unbounded channel is fire-and-forget from every caller's
/// perspective -- the `send` itself never waits for the write, so NO
/// caller (relay loop included) is ever suspended by another writer's I/O,
/// no matter which arm is submitting or how long the queue is.
/// Returns the request sender plus the worker's own `JoinHandle` -- PAN-61
/// review round 8 (Codex P1): the caller MUST register the handle in
/// `named_task_handles` so graceful shutdown awaits (with its existing
/// bounded per-task timeout) this worker draining any mutations already
/// queued before it exits, instead of the runtime silently cancelling it
/// mid-drain when the process tears down.
fn spawn_config_file_write_worker() -> (
    tokio::sync::mpsc::UnboundedSender<ConfigFileWriteRequest>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ConfigFileWriteRequest>();
    let handle = tokio::spawn(async move {
        while let Some(req) = rx.recv().await {
            let result = match tokio::task::spawn_blocking(req.work).await {
                Ok(result) => result,
                Err(join_err) => Err(pancetta_config::ConfigError::Validation(format!(
                    "config file write task panicked or was cancelled: {}",
                    join_err
                ))),
            };
            let _ = req.respond.send(result);
        }
        Ok(())
    });
    (tx, handle)
}

/// One bookmark mutation, queued for serialized processing by the worker
/// `spawn_bookmark_mutation_worker` spawns (PAN-61 review round 7, P1).
///
/// `spawn_config_file_write_worker` above serializes the raw FILE WRITE
/// across all 4 targeted writers in this coordinator, which prevents
/// `write_secure_atomic` corruption -- but it does nothing to stop two
/// concurrent bookmark commands from both staging their edit off the same
/// pre-mutation `cfg.rig.bookmarks` snapshot, in which case the later
/// commit would silently discard the earlier one's change even though the
/// disk writes themselves never interleaved. This worker is the other
/// half: a single task processes one mutation fully (stage, submit to the
/// file-write worker, await its result, commit to shared state) before
/// starting the next, so every stage-read is guaranteed fresh.
enum BookmarkMutation {
    Save(pancetta_config::rig::RigBookmark),
    Delete(String),
}

/// Spawn the single serialized worker that processes every
/// `SaveRigBookmark`/`DeleteRigBookmark` mutation one at a time, and
/// return the sender the relay loop submits mutations through plus the
/// worker's own `JoinHandle`. The relay loop's own arms do nothing but
/// build the request and `send` it -- never awaited, so they return to
/// `try_recv` immediately regardless of how long this worker's queue
/// takes to drain.
///
/// PAN-61 review round 8 (Codex P1): the caller MUST register the
/// returned handle in `named_task_handles` -- see
/// `spawn_config_file_write_worker`'s identical note.
fn spawn_bookmark_mutation_worker(
    config: std::sync::Arc<tokio::sync::RwLock<pancetta_config::Config>>,
    write_tx: tokio::sync::mpsc::UnboundedSender<ConfigFileWriteRequest>,
    tui_msg_tx: crossbeam_channel::Sender<pancetta_tui::tui_runner::TuiMessage>,
    config_path: std::path::PathBuf,
    // PAN-62 review round 1 (Codex P2) / round 2 (Codex P1):
    // `set_rig_bookmarks_in_file` parses the existing file exclusively as
    // TOML -- taken as a precomputed argument (never checked here via
    // synchronous disk I/O) so this stays consistent with `main.rs`'s
    // `config_write_is_toml`, the single source of truth for the format
    // detection that must match what `load_configuration_with_warnings`
    // actually parsed.
    config_is_toml: bool,
) -> (
    tokio::sync::mpsc::UnboundedSender<BookmarkMutation>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<BookmarkMutation>();
    let handle = tokio::spawn(async move {
        while let Some(mutation) = rx.recv().await {
            // Bookmarks have no "live apply" fallback the way SelectDevice/
            // SelectRig do -- a save/delete IS the persist, so a non-TOML
            // config file means the whole mutation is rejected outright
            // rather than reporting a confusing raw parser failure.
            if !config_is_toml {
                let name = match &mutation {
                    BookmarkMutation::Save(bookmark) => bookmark.name.clone(),
                    BookmarkMutation::Delete(name) => name.clone(),
                };
                warn!(
                    "Config file {} is not TOML; bookmark '{}' not saved/deleted",
                    config_path.display(),
                    name
                );
                let _ = tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                    component: "rig".to_string(),
                    status: "Bookmarks aren't supported for a non-TOML config file".to_string(),
                });
                continue;
            }
            // Stage fresh at the top of every iteration -- this only runs
            // after the PREVIOUS mutation (if any) fully committed below,
            // since this loop processes one item at a time.
            let mut staged = {
                let cfg = config.read().await;
                cfg.rig.effective_bookmarks().to_vec()
            };

            let name = match &mutation {
                BookmarkMutation::Save(bookmark) => bookmark.name.clone(),
                BookmarkMutation::Delete(name) => name.clone(),
            };

            match &mutation {
                BookmarkMutation::Save(bookmark) => {
                    upsert_rig_bookmark(&mut staged, bookmark.clone());
                }
                BookmarkMutation::Delete(_) => {
                    let before = staged.len();
                    staged.retain(|b| b.name != name);
                    if staged.len() == before {
                        let _ =
                            tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                component: "rig".to_string(),
                                status: format!("No bookmark named '{}'", name),
                            });
                        continue;
                    }
                }
            }

            let (resp_tx, resp_rx) = tokio::sync::oneshot::channel();
            let write_path = config_path.clone();
            let write_staged = staged.clone();
            if write_tx
                .send(ConfigFileWriteRequest {
                    work: Box::new(move || {
                        pancetta_config::Config::default()
                            .set_rig_bookmarks_in_file(&write_path, &write_staged)
                    }),
                    respond: resp_tx,
                })
                .is_err()
            {
                warn!(
                    "Config write worker channel closed; bookmark '{}' change not persisted",
                    name
                );
                continue;
            }
            let persist_result = match resp_rx.await {
                Ok(result) => result,
                Err(_) => Err(pancetta_config::ConfigError::Validation(
                    "config write worker dropped the response".to_string(),
                )),
            };

            let verb = match &mutation {
                BookmarkMutation::Save(_) => "save",
                BookmarkMutation::Delete(_) => "delete",
            };
            let status = if let Err(e) = persist_result {
                warn!("Failed to persist rig bookmark {}: {}", verb, e);
                format!("Failed to {} bookmark '{}': {}", verb, name, e)
            } else {
                info!(
                    "Persisted rig bookmark {} ('{}') to {}",
                    verb,
                    name,
                    config_path.display()
                );
                {
                    let mut cfg = config.write().await;
                    // PAN-63: always `Some`, even for a delete-to-empty --
                    // this in-memory commit is an explicit "the operator
                    // set bookmarks to this list", so the on-disk write
                    // this mirrors (`set_rig_bookmarks_in_file`, below)
                    // must not let a lower-priority source's list show
                    // through on the next config load's merge.
                    cfg.rig.bookmarks = Some(staged.clone());
                }
                let _ = tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::RigBookmarksUpdate {
                    bookmarks: staged.clone(),
                });
                match &mutation {
                    BookmarkMutation::Save(_) => rig_bookmark_saved_status(&name, staged.len()),
                    BookmarkMutation::Delete(_) => format!("Deleted bookmark '{}'", name),
                }
            };
            let _ = tui_msg_tx.send(pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                component: "rig".to_string(),
                status,
            });
        }
        Ok(())
    });
    (tx, handle)
}

#[cfg(test)]
mod tui_relay_tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU8};

    /// PAN-72: `NudgeTxOffset` with an active QSO must force a `Switch`
    /// onto the EXISTING `pending_qso_offset_requests` mailbox (Task 8/9's
    /// pipeline) -- not a new commit path -- and must NOT also set the
    /// CQ-hunting fallback flag.
    #[test]
    fn nudge_forces_switch_for_active_qso() {
        let qso_id = pancetta_qso::QsoId::new_v4();
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([key.clone()]);
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key.clone(), 920.0)]));
        let pending_qso_offset_requests: std::sync::Mutex<
            Vec<(
                pancetta_qso::states::QsoId,
                pancetta_qso::qso_manager::OffsetAction,
            )>,
        > = std::sync::Mutex::new(Vec::new());
        let pending_cq_offset_nudge = AtomicBool::new(false);

        let result = resolve_nudge_tx_offset(
            &active,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(result, Some(qso_id), "must resolve to the active QSO's id");
        let pending = pending_qso_offset_requests.lock().unwrap();
        assert_eq!(pending.len(), 1, "exactly one offset request queued");
        assert_eq!(pending[0].0, qso_id);
        assert!(
            matches!(
                pending[0].1,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz } if avoid_hz == 920.0
            ),
            "must be a Switch{{avoid_hz}} keyed off the QSO's CURRENT offset, got {:?}",
            pending[0].1
        );
        assert!(
            !pending_cq_offset_nudge.load(Ordering::Relaxed),
            "the CQ-hunting fallback flag must NOT be set when an active QSO was nudged"
        );
    }

    /// PAN-72: `NudgeTxOffset` with no active QSO must set the one-shot
    /// `pending_cq_offset_nudge` flag (the CQ-hunting fallback) and must NOT
    /// touch `pending_qso_offset_requests`.
    #[test]
    fn nudge_sets_cq_flag_when_no_active_qso() {
        let active: std::collections::HashSet<String> = std::collections::HashSet::new();
        let active_tx_offsets = std::sync::RwLock::new(std::collections::HashMap::new());
        let pending_qso_offset_requests: std::sync::Mutex<
            Vec<(
                pancetta_qso::states::QsoId,
                pancetta_qso::qso_manager::OffsetAction,
            )>,
        > = std::sync::Mutex::new(Vec::new());
        let pending_cq_offset_nudge = AtomicBool::new(false);

        let result = resolve_nudge_tx_offset(
            &active,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(result, None, "no active QSO to resolve to");
        assert!(
            pending_cq_offset_nudge.load(Ordering::Relaxed),
            "the CQ-hunting fallback flag must be set when no QSO is active"
        );
        assert!(
            pending_qso_offset_requests.lock().unwrap().is_empty(),
            "must not queue an offset request when there's no active QSO"
        );
    }

    /// An empty pending slot -- the common case (nothing carried over from
    /// a prior failed teardown replay).
    fn no_pending() -> Arc<std::sync::Mutex<Option<ComponentMessage>>> {
        Arc::new(std::sync::Mutex::new(None))
    }

    /// The frequency sibling of `no_pending()` -- PAN-35 keyed the
    /// frequency pending slot by VFO, so its empty state is an empty map
    /// rather than `None`.
    fn no_pending_frequency(
    ) -> Arc<std::sync::Mutex<std::collections::HashMap<pancetta_hamlib::Vfo, ComponentMessage>>>
    {
        Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()))
    }

    /// Not in flight -- the common case (no CAT call currently executing).
    fn not_in_flight() -> Arc<std::sync::atomic::AtomicU32> {
        Arc::new(std::sync::atomic::AtomicU32::new(0))
    }

    /// PAN-67: `decode_view_dial_mhz` takes no live-state argument at all —
    /// it can only report the frequency captured with the decode's own
    /// audio window, so a live band switch happening after that window
    /// closed cannot leak into the displayed frequency.
    #[test]
    fn decode_view_dial_mhz_reports_the_relayed_windows_own_frequency() {
        let message = pancetta_ft8::DecodedMessage::new(
            pancetta_ft8::Ft8Message::default(),
            -10.0,
            0.5,
            1500.0,
            0.1,
        );
        let old_band = crate::coordinator::pipeline::RelayedDecode {
            message: message.clone(),
            dial_hz: 7_074_000,
        };
        let new_band = crate::coordinator::pipeline::RelayedDecode {
            message,
            dial_hz: 14_074_000,
        };

        assert_eq!(decode_view_dial_mhz(&old_band), 7.074);
        assert_eq!(decode_view_dial_mhz(&new_band), 14.074);
    }

    /// PAN-67 review round 1 finding 2: an unestablished dial (no rig, no
    /// CAT read yet, or a failed read) must fall back to 14.074 MHz — the
    /// same default the removed live atomic seeded — never stamp 0.0 MHz
    /// and feed 0 Hz into the worked/needed lookups.
    #[test]
    fn decode_view_dial_mhz_falls_back_to_default_when_dial_is_unknown() {
        let message = pancetta_ft8::DecodedMessage::new(
            pancetta_ft8::Ft8Message::default(),
            -10.0,
            0.5,
            1500.0,
            0.1,
        );
        let unknown_dial = crate::coordinator::pipeline::RelayedDecode {
            message,
            dial_hz: 0,
        };

        assert_eq!(decode_view_dial_mhz(&unknown_dial), 14.074);
    }

    #[test]
    fn upsert_rig_bookmark_appends_when_name_is_new() {
        let mut bookmarks = vec![pancetta_config::rig::RigBookmark {
            name: "Shack".to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 38400,
            ptt_method: pancetta_config::rig::PttMethod::Cat,
        }];
        upsert_rig_bookmark(
            &mut bookmarks,
            pancetta_config::rig::RigBookmark {
                name: "Portable".to_string(),
                model: "IC-7300".to_string(),
                port: "/dev/ttyUSB1".to_string(),
                baud_rate: 19200,
                ptt_method: pancetta_config::rig::PttMethod::Vox,
            },
        );
        assert_eq!(bookmarks.len(), 2);
        assert_eq!(bookmarks[1].name, "Portable");
    }

    #[test]
    fn upsert_rig_bookmark_overwrites_when_name_matches() {
        let mut bookmarks = vec![pancetta_config::rig::RigBookmark {
            name: "Shack".to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 38400,
            ptt_method: pancetta_config::rig::PttMethod::Cat,
        }];
        upsert_rig_bookmark(
            &mut bookmarks,
            pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "IC-7300".to_string(),
                port: "/dev/ttyUSB1".to_string(),
                baud_rate: 19200,
                ptt_method: pancetta_config::rig::PttMethod::Vox,
            },
        );
        assert_eq!(
            bookmarks.len(),
            1,
            "must overwrite, not append a duplicate name"
        );
        assert_eq!(bookmarks[0].model, "IC-7300");
    }

    /// PAN-62 review round 1 (Codex P2): saving a bookmark against a JSON
    /// `--config` file must be rejected with a clear operator-facing
    /// message -- not silently accepted (the file is never actually
    /// written, since `set_rig_bookmarks_in_file` only understands TOML)
    /// and not a raw "failed to parse config" parser error.
    #[tokio::test]
    async fn bookmark_worker_rejects_a_save_against_a_json_config_file() {
        let dir = tempfile::tempdir().unwrap();
        let json_path = dir.path().join("pancetta.json");
        std::fs::write(&json_path, "{}").unwrap();

        let config = Arc::new(tokio::sync::RwLock::new(pancetta_config::Config::default()));
        let (write_tx, _write_handle) = spawn_config_file_write_worker();
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let (bookmark_tx, _bookmark_handle) = spawn_bookmark_mutation_worker(
            config.clone(),
            write_tx,
            tui_msg_tx,
            json_path.clone(),
            false, // config_is_toml: mirrors what main.rs's config_write_is_toml
                   // would compute for this .json path
        );

        bookmark_tx
            .send(BookmarkMutation::Save(pancetta_config::rig::RigBookmark {
                name: "Shack".to_string(),
                model: "FTdx10".to_string(),
                port: "/dev/ttyUSB0".to_string(),
                baud_rate: 38400,
                ptt_method: pancetta_config::rig::PttMethod::None,
            }))
            .unwrap();

        // `crossbeam_channel::Receiver::recv()` blocks the OS thread (not
        // just the future), which would starve `tokio::time::timeout`'s own
        // cancellation -- poll with `try_recv` + an async sleep instead so
        // the runtime keeps making progress.
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(pancetta_tui::tui_runner::TuiMessage::StatusUpdate { status, .. }) =
                    tui_msg_rx.try_recv()
                {
                    return status;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("worker must report a status within 5s");

        assert!(
            msg.contains("aren't supported") && msg.contains("non-TOML"),
            "expected a clear non-TOML rejection message, got: {msg}"
        );
        assert_eq!(
            std::fs::read_to_string(&json_path).unwrap(),
            "{}",
            "the JSON file must be left completely untouched"
        );
        assert!(
            config.read().await.rig.effective_bookmarks().is_empty(),
            "in-memory bookmarks must NOT reflect a rejected save"
        );
    }

    #[test]
    fn rig_bookmark_saved_status_under_cap_has_no_warning() {
        let status = rig_bookmark_saved_status("Shack", 5);
        assert_eq!(status, "Saved bookmark 'Shack'");
    }

    #[test]
    fn rig_bookmark_saved_status_at_cap_warns_but_still_reports_success() {
        let status = rig_bookmark_saved_status("Shack", 20);
        assert!(status.starts_with("Saved bookmark 'Shack'"));
        assert!(status.contains("consider deleting"));
    }

    /// PAN-19 round-12 review (Codex P1) regression guard: "apply loop
    /// readiness to direct PTT commands". The exact scenario the finding
    /// describes -- `start_hamlib_component` returned from a
    /// `LoopReadyOutcome::TimedOut` (restart-supervision inhibit already
    /// released back to 0) but `hamlib_command_loop_ready` never got set
    /// true. Before this fix, `TogglePtt`'s own separate check only looked
    /// at `tx_restart_inhibit` and TX policy, so this exact state (policy
    /// Full, restart_inhibit 0, loop NOT ready) would have let a manual
    /// PTT-on through -- queuing a key-up the slow loop could consume
    /// later, unexpectedly keying the radio well after the operator's
    /// keypress. `ptt_on_refusal` must refuse it.
    #[test]
    fn ptt_on_refusal_blocks_a_key_up_while_the_hamlib_loop_is_not_yet_ready() {
        let tx_policy = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8()));
        let tx_restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_command_loop_ready = Arc::new(AtomicBool::new(false));

        let refusal = ptt_on_refusal(
            &tx_policy,
            &tx_restart_inhibit,
            &hamlib_command_loop_ready,
            &no_pending_frequency(),
            &no_pending(),
            &not_in_flight(),
        );
        assert!(
            refusal.is_some(),
            "a direct PTT-on toggle must be refused while the Hamlib command loop hasn't \
             confirmed readiness, even though the restart-supervision inhibit counter has \
             already released"
        );
    }

    /// PAN-19 round-14 review (Codex P1) regression guard: "keep TX muted
    /// until pending rig state is delivered". A pending `SetSplit` still
    /// sitting undelivered (channel was momentarily full at startup, round
    /// 10's requeue mechanism) with `hamlib_command_loop_ready == true` and
    /// `tx_restart_inhibit == 0` -- the exact gap the finding describes,
    /// where the loop is ready to consume commands but the rig's split
    /// state is still stale. A direct PTT-on toggle must be refused until
    /// the pending item clears, then permitted once it does.
    #[test]
    fn ptt_on_refusal_blocks_a_key_up_while_a_pending_split_is_undelivered() {
        let tx_policy = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8()));
        let tx_restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_command_loop_ready = Arc::new(AtomicBool::new(true));
        let pending_split = Arc::new(std::sync::Mutex::new(Some(ComponentMessage::new(
            ComponentId::Hamlib,
            ComponentId::Hamlib,
            MessageType::RigControl(crate::message_bus::RigControlMessage::SetSplit {
                enabled: true,
                tx_frequency: 14_078_000,
            }),
            std::time::Instant::now(),
        ))));

        assert!(
            ptt_on_refusal(
                &tx_policy,
                &tx_restart_inhibit,
                &hamlib_command_loop_ready,
                &no_pending_frequency(),
                &pending_split,
                &not_in_flight(),
            )
            .is_some(),
            "a direct PTT-on toggle must be refused while a pending SetSplit is still \
             undelivered, even though the Hamlib command loop is ready and restart isn't \
             inhibiting"
        );

        // Once delivered (or applied and the slot drained), PTT-on must be
        // permitted again.
        *pending_split.lock().unwrap() = None;
        assert_eq!(
            ptt_on_refusal(
                &tx_policy,
                &tx_restart_inhibit,
                &hamlib_command_loop_ready,
                &no_pending_frequency(),
                &pending_split,
                &not_in_flight(),
            ),
            None,
            "PTT-on must be permitted again once the pending SetSplit has been delivered"
        );
    }

    /// PAN-19 round-16 review (Codex P1) regression guard: "keep restored
    /// rig state pending through CAT application". Both pending slots are
    /// ALREADY empty (mirrors the exact race: `deliver_pending_hamlib_state`
    /// cleared the slot at hand-off, before the message loop's CAT call
    /// even started), the loop is ready, and restart isn't inhibiting --
    /// yet a CAT call is genuinely in flight right now. A direct PTT-on
    /// toggle must still be refused, then permitted again once the
    /// in-flight flag clears.
    #[test]
    fn ptt_on_refusal_blocks_a_key_up_while_a_rig_command_is_in_flight() {
        let tx_policy = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8()));
        let tx_restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_command_loop_ready = Arc::new(AtomicBool::new(true));
        let in_flight = Arc::new(std::sync::atomic::AtomicU32::new(1));

        assert!(
            ptt_on_refusal(
                &tx_policy,
                &tx_restart_inhibit,
                &hamlib_command_loop_ready,
                &no_pending_frequency(),
                &no_pending(),
                &in_flight,
            )
            .is_some(),
            "a direct PTT-on toggle must be refused while a rig frequency/split command is in \
             flight, even though both pending slots are already empty"
        );

        in_flight.store(0, Ordering::Release);
        assert_eq!(
            ptt_on_refusal(
                &tx_policy,
                &tx_restart_inhibit,
                &hamlib_command_loop_ready,
                &no_pending_frequency(),
                &no_pending(),
                &in_flight,
            ),
            None,
            "PTT-on must be permitted again once the in-flight CAT call has resolved"
        );
    }

    /// The flip side: once everything is genuinely ready, PTT-on must be
    /// allowed through -- the fix must not become overly conservative.
    #[test]
    fn ptt_on_refusal_permits_a_key_up_when_everything_is_ready() {
        let tx_policy = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8()));
        let tx_restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_command_loop_ready = Arc::new(AtomicBool::new(true));

        assert_eq!(
            ptt_on_refusal(
                &tx_policy,
                &tx_restart_inhibit,
                &hamlib_command_loop_ready,
                &no_pending_frequency(),
                &no_pending(),
                &not_in_flight(),
            ),
            None,
            "PTT-on must be permitted once TX policy allows it, restart isn't inhibiting, and \
             the Hamlib command loop has confirmed readiness"
        );
    }

    /// PAN-35 regression guard: a pending command for just ONE VFO must
    /// still refuse a manual PTT key-up -- the pending slot is keyed by
    /// VFO now, so `ptt_on_refusal` (via `tx_hard_mute_reason`) must check
    /// the map as a whole rather than assuming a specific VFO's entry.
    #[test]
    fn ptt_on_refusal_blocks_a_key_up_while_either_vfo_alone_is_pending() {
        let tx_policy = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8()));
        let tx_restart_inhibit = Arc::new(AtomicU32::new(0));
        let hamlib_command_loop_ready = Arc::new(AtomicBool::new(true));

        for vfo in [pancetta_hamlib::Vfo::A, pancetta_hamlib::Vfo::B] {
            let mut pending_frequency_map = std::collections::HashMap::new();
            pending_frequency_map.insert(
                vfo,
                ComponentMessage::new(
                    ComponentId::Hamlib,
                    ComponentId::Hamlib,
                    MessageType::RigControl(crate::message_bus::RigControlMessage::SetFrequency {
                        vfo: if vfo == pancetta_hamlib::Vfo::A { 0 } else { 1 },
                        frequency: 14_074_000,
                    }),
                    std::time::Instant::now(),
                ),
            );
            let pending_frequency = Arc::new(std::sync::Mutex::new(pending_frequency_map));

            assert!(
                ptt_on_refusal(
                    &tx_policy,
                    &tx_restart_inhibit,
                    &hamlib_command_loop_ready,
                    &pending_frequency,
                    &no_pending(),
                    &not_in_flight(),
                )
                .is_some(),
                "a pending command for {vfo:?} alone must still refuse a manual PTT key-up"
            );
        }
    }

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
            dx_last_activity: None,
            hound: false,
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
            dx_last_activity: None,
            hound: false,
        };
        let banner = map_qso_snapshot_item(&item);
        assert!(banner.last_tx_text.is_none());
        assert!(banner.last_rx_text.is_none());
        assert!(banner.snr_rx.is_none());
        assert!(banner.report_sent.is_none());
        assert!(banner.report_received.is_none());
        assert_eq!(banner.exchange_count, 0);
    }

    /// Layer 2 Recent-QSOs relay: a completed outcome carries every field
    /// through field-for-field, mirroring
    /// `map_qso_snapshot_item_carries_all_detail_fields` above.
    #[test]
    fn map_recent_qso_outcome_carries_completed_fields() {
        let ts = chrono::Utc::now();
        let outcome = crate::message_bus::RecentQsoOutcome {
            callsign: "JA1ABC".to_string(),
            outcome: crate::message_bus::QsoOutcome::Completed,
            last_state: "Completed".to_string(),
            freq_hz: 1500,
            ts,
            brief_timeline: vec!["QSO with JA1ABC logged (RST -10/-05)".to_string()],
        };
        let mapped = map_recent_qso_outcome(&outcome);
        assert_eq!(mapped.callsign, "JA1ABC");
        assert!(matches!(
            mapped.outcome,
            pancetta_tui::app::QsoOutcome::Completed
        ));
        assert_eq!(mapped.last_state, "Completed");
        assert_eq!(mapped.freq_hz, 1500);
        assert_eq!(mapped.ts, ts);
        assert_eq!(
            mapped.brief_timeline,
            vec!["QSO with JA1ABC logged (RST -10/-05)".to_string()]
        );
    }

    /// A failed outcome carries its `QsoFailureReason` through unconverted.
    #[test]
    fn map_recent_qso_outcome_carries_failure_reason() {
        let outcome = crate::message_bus::RecentQsoOutcome {
            callsign: "W2XYZ".to_string(),
            outcome: crate::message_bus::QsoOutcome::Failed(
                pancetta_qso::QsoFailureReason::Timeout,
            ),
            last_state: "Failed".to_string(),
            freq_hz: 900,
            ts: chrono::Utc::now(),
            brief_timeline: vec!["QSO failed: timeout".to_string()],
        };
        let mapped = map_recent_qso_outcome(&outcome);
        assert!(matches!(
            mapped.outcome,
            pancetta_tui::app::QsoOutcome::Failed(pancetta_qso::QsoFailureReason::Timeout)
        ));
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

    /// Display priority scorer must produce DISTINCT scores for stations
    /// with different rarity/SNR signals — verifying the relay thread's
    /// PriorityScorer produces continuous f64 values that map to varied
    /// [0,1000] display scores rather than the historic 0/500/1000 buckets.
    #[test]
    fn relay_display_scorer_produces_distinct_scores() {
        use pancetta_qso::priority::{PriorityScorer, PriorityWeights};
        use pancetta_qso::DxEvaluator;

        let lookup = crate::priority_evaluator::CachedStationLookup::new();

        // Set up: rare station in needed-DXCC set.
        let mut needed = std::collections::HashSet::new();
        needed.insert("JA".to_string());
        lookup.update_needed_dxcc(needed);
        let mut rarity = std::collections::HashMap::new();
        rarity.insert("JA1ABC".to_string(), 0.85); // high rarity
        rarity.insert("W1XYZ".to_string(), 0.2); // low rarity
        lookup.update_rarity_scores(rarity);

        let scorer = PriorityScorer::new(PriorityWeights::default(), Box::new(lookup));

        let score_needed_rare = scorer.evaluate_cq("JA1ABC", Some("PM95"), -10, 14_074_000.0);
        let score_plain_common = scorer.evaluate_cq("W1XYZ", Some("FN20"), -22, 14_074_000.0);
        let score_plain_moderate = scorer.evaluate_cq("W1XYZ", Some("FN20"), -5, 14_074_000.0);

        // Needed+rare must clearly outrank plain common.
        assert!(
            score_needed_rare > score_plain_common,
            "needed+rare ({score_needed_rare:.3}) must outrank plain common ({score_plain_common:.3})"
        );

        // Two plain stations at different SNR must score differently (continuous).
        assert!(
            score_plain_moderate > score_plain_common,
            "SNR -5 ({score_plain_moderate:.3}) must outrank SNR -22 ({score_plain_common:.3})"
        );

        // The scores must NOT be the same as the old coarse buckets (0.0/0.5/1.0).
        assert!(
            score_needed_rare > 0.0 && score_needed_rare < 1.0,
            "score must be a continuous value in (0,1), got {score_needed_rare}"
        );

        // Map to display u32 — should produce distinct values, not all-zero.
        let display_needed_rare = (score_needed_rare * 1000.0).round() as u32;
        let display_plain_common = (score_plain_common * 1000.0).round() as u32;
        assert_ne!(
            display_needed_rare, display_plain_common,
            "display scores must be distinct: {display_needed_rare} vs {display_plain_common}"
        );
    }
}
