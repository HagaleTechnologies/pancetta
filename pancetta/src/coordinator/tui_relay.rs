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

/// Per-callsign decode memory kept by the relay loop. Entries live as long as
/// the TUI keeps a DX Hunter row (`App::cleanup_old_data`: 24 h), and the map
/// holds at most `RELAY_CACHE_CAP` entries, dropping the oldest first, so a
/// busy band or decoder-noise callsigns cannot grow it without limit.
const RELAY_CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);
const RELAY_CACHE_CAP: usize = 4096;

struct RelayCache<K, V> {
    entries: std::collections::HashMap<K, (V, Instant)>,
    ttl: Duration,
    cap: usize,
}

impl<K: std::hash::Hash + Eq + Clone, V> Default for RelayCache<K, V> {
    fn default() -> Self {
        Self::new(RELAY_CACHE_TTL, RELAY_CACHE_CAP)
    }
}

impl<K: std::hash::Hash + Eq + Clone, V> RelayCache<K, V> {
    fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            entries: std::collections::HashMap::new(),
            ttl,
            cap,
        }
    }

    fn live(&self, at: Instant, now: Instant) -> bool {
        now.saturating_duration_since(at) <= self.ttl
    }

    fn insert(&mut self, key: K, value: V) {
        self.insert_at(key, value, Instant::now());
    }

    fn insert_at(&mut self, key: K, value: V, now: Instant) {
        self.entries.insert(key, (value, now));
        if self.entries.len() <= self.cap {
            return;
        }
        let ttl = self.ttl;
        self.entries
            .retain(|_, (_, at)| now.saturating_duration_since(*at) <= ttl);
        while self.entries.len() > self.cap {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, at))| *at)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn get(&self, key: &K) -> Option<&V> {
        self.get_at(key, Instant::now())
    }

    fn get_at(&self, key: &K, now: Instant) -> Option<&V> {
        self.entries
            .get(key)
            .filter(|(_, at)| self.live(*at, now))
            .map(|(value, _)| value)
    }

    fn iter(&self) -> impl Iterator<Item = (&K, &V)> {
        let now = Instant::now();
        self.entries
            .iter()
            .filter(move |(_, (_, at))| self.live(*at, now))
            .map(|(key, (value, _))| (key, value))
    }

    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// Keep the last decoded grid for this exact callsign and band during the relay
/// session. Report/acknowledgment frames omit it; a later grid replaces it.
fn display_grid(
    last_grids: &mut RelayCache<(String, String), String>,
    call_sign: Option<&str>,
    grid: Option<&str>,
    dial_mhz: f64,
) -> Option<String> {
    let Some(call) = call_sign else {
        return grid.map(str::to_owned);
    };
    let key = (
        call.to_owned(),
        pancetta_qso::utils::frequency_to_band(dial_mhz * 1_000_000.0),
    );
    if let Some(grid) = grid {
        last_grids.insert(key, grid.to_owned());
        Some(grid.to_owned())
    } else {
        last_grids.get(&key).cloned()
    }
}

/// Inputs of a station's latest decode, per exact callsign (PAN-94).
#[derive(Debug, Clone)]
struct RankInputs {
    grid: Option<String>,
    snr_db: f32,
    dial_mhz: f64,
}

/// Rescore every row a terminal QSO outcome names from its latest decode's
/// inputs, so worked and failure history recorded at QSO end reach DX Hunter
/// before that station is decoded again. Rows match on resolved identity, so
/// a hash-rendered decode (`<PJ4/K1ABC>`) and the QSO's plain callsign find
/// each other in either direction. Empty when no matching callsign has been
/// decoded in this relay session.
fn outcome_rank_refresh(
    scorer: &pancetta_qso::PriorityScorer,
    last_inputs: &RelayCache<String, RankInputs>,
    callsign: &str,
) -> Vec<pancetta_tui::tui_runner::TuiMessage> {
    let identity = |call: &str| {
        pancetta_core::callsign::resolve_hash_render(&call.trim().to_uppercase()).map(str::to_owned)
    };
    let Some(target) = identity(callsign) else {
        return Vec::new();
    };
    last_inputs
        .iter()
        .filter(|(call, _)| identity(call).as_deref() == Some(target.as_str()))
        .filter_map(|(call, inputs)| {
            let (Some(score), Some(factors)) = display_rank(
                scorer,
                Some(call),
                inputs.grid.as_deref(),
                inputs.snr_db,
                inputs.dial_mhz,
            ) else {
                return None;
            };
            Some(pancetta_tui::tui_runner::TuiMessage::DxRankRefresh {
                callsign: call.clone(),
                score,
                factors,
            })
        })
        .collect()
}

/// Capture score and explanation in one scoring call over the shared lookup.
fn display_rank(
    scorer: &pancetta_qso::PriorityScorer,
    call_sign: Option<&str>,
    grid: Option<&str>,
    snr_db: f32,
    dial_mhz: f64,
) -> (Option<u32>, Option<pancetta_qso::priority::PriorityFactors>) {
    let Some(call) = call_sign else {
        return (None, None);
    };
    let explained = scorer.score_tiered_explained(
        call,
        grid,
        snr_db.round().clamp(-128.0, 127.0) as i8,
        dial_mhz * 1_000_000.0,
    );
    (
        Some(explained.score.as_display_u32()),
        Some(explained.factors),
    )
}

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
        // PAN-114: clock-skew warning published by the clock-skew monitor
        // (`clock_monitor.rs`), read on the same 2s health tick.
        let clock_skew_relay = self.clock_skew.clone();
        // PAN-89: readiness-strip inputs, snapshotted on the same 2s tick.
        let readiness_sources = super::readiness::ReadinessSources::from_coordinator(self);
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
            // PAN-89: current RF/silent-input edge state for the readiness
            // snapshot, and its push-on-change bookkeeping.
            let mut rf_no_decode_active = false;
            let mut input_silent_active = false;
            let mut readiness_publisher = super::readiness::ReadinessPublisher::default();
            // PAN-85: remember the last US state actually forwarded for each
            // callsign (uppercased key) rather than merely that one was sent,
            // so both a state learned later (QRZ enrichment mid-session) and a
            // state *corrected* later (log-seeded code superseded by QRZ)
            // reach the TUI on the next decode/spot for that callsign. Repeats
            // of an unchanged value are still suppressed.
            let mut sent_states: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut last_grids = RelayCache::default();
            let mut last_rank_inputs = RelayCache::default();
            while !relay_shutdown.load(Ordering::Acquire) {
                if !ft8_disconnected {
                    match ft8_to_tui_rx.try_recv() {
                        Ok(relayed) => {
                            let decoded_msg = &relayed.message;
                            let call_sign = decoded_msg.message.from_callsign.clone();
                            let dial_mhz = decode_view_dial_mhz(&relayed);
                            let grid_square = display_grid(
                                &mut last_grids, call_sign.as_deref(),
                                decoded_msg.message.grid_square.as_deref(), dial_mhz);
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

                            // PAN-85: forward a known US state whenever it is
                            // new or has changed since the last one sent.
                            if let Some(call) = call_sign.as_deref() {
                                if let Some(state) = state_for(&relay_station_lookup, Some(call)) {
                                    let key = call.to_uppercase();
                                    if sent_states.get(&key) != Some(&state) {
                                        sent_states.insert(key, state.clone());
                                        let _ = tui_msg_tx_relay.send(
                                            pancetta_tui::tui_runner::TuiMessage::StationState {
                                                callsign: call.to_string(),
                                                state,
                                            },
                                        );
                                    }
                                }
                            }

                            // Compute the #164 tiered priority score via the
                            // real PriorityScorer's classification (ATNO >
                            // per-band-DXCC-new > special-station >
                            // per-band-grid-new > everything else, encoded
                            // to a single sortable u32 — see
                            // TieredScore::as_display_u32). Every identified
                            // frame updates DX Hunter, using the retained
                            // grid and fresh signal/worked/failure inputs.
                            let (priority_score, priority_factors) = display_rank(
                                &relay_scorer, call_sign.as_deref(), grid_square.as_deref(),
                                decoded_msg.snr_db, dial_mhz);
                            if let Some(call) = call_sign.as_deref() {
                                last_rank_inputs.insert(call.to_owned(), RankInputs {
                                    grid: grid_square.clone(),
                                    snr_db: decoded_msg.snr_db,
                                    dial_mhz,
                                });
                            }

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
                                priority_factors,
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
                            // PAN-85: forward a known US state whenever it is
                            // new or has changed since the last one sent.
                            if let Some(state) =
                                state_for(&relay_station_lookup, Some(callsign.as_str()))
                            {
                                let key = callsign.to_uppercase();
                                if sent_states.get(&key) != Some(&state) {
                                    sent_states.insert(key, state.clone());
                                    let _ = tui_msg_tx_relay.send(
                                        pancetta_tui::tui_runner::TuiMessage::StationState {
                                            callsign: callsign.clone(),
                                            state,
                                        },
                                    );
                                }
                            }
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
                            // PAN-94: the QSO task records worked/failure
                            // history before emitting this outcome, so rescore
                            // the row now rather than at its next decode.
                            for refresh in outcome_rank_refresh(
                                &relay_scorer, &last_rank_inputs, &outcome.callsign)
                            {
                                let _ = tui_msg_tx_relay.send(refresh);
                            }
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
                        clock_skew_warning_s: clock_skew_relay.warning(),
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
                    if let Some(active) = edges.rf_no_decode {
                        rf_no_decode_active = active;
                    }
                    if let Some(active) = edges.silent_input {
                        input_silent_active = active;
                    }
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

                    // PAN-89: station-readiness strip — push only on change.
                    let readiness =
                        readiness_sources.snapshot(rf_no_decode_active, input_silent_active);
                    let (send_readiness, ptt_cleared) = readiness_publisher.observe(&readiness);
                    if send_readiness {
                        let _ = tui_msg_tx_relay.send(
                            pancetta_tui::tui_runner::TuiMessage::StationReadiness(readiness),
                        );
                    }
                    if ptt_cleared {
                        const PTT_CLEARED: &str =
                            "PTT-off confirmed by the rig — stuck-PTT warning cleared";
                        info!("{PTT_CLEARED}");
                        let _ = tui_msg_tx_relay.send(
                            pancetta_tui::tui_runner::TuiMessage::DiagnosticEvent {
                                ts: chrono::Utc::now(),
                                target: "supervisor",
                                level: pancetta_core::DiagnosticLevel::Info,
                                text: PTT_CLEARED.into(),
                                qso_id: None,
                                callsign: None,
                            },
                        );
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
        // PAN-118: band-plan snapshot for the manual-PTT band gate
        // (`ptt_band_plan_refusal`). Config loads once at startup (the
        // hot-reload apply task is a documented no-op), so a relay-startup
        // snapshot is always the live value.
        let cmd_band_plan = {
            let cfg = self.config.read().await;
            cfg.rig.frequency.band_plan.clone()
        };
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
        // PAN-115: rig CAT link / audio output health -- see
        // `tx_hard_mute_reason`'s doc comment in `coordinator/tx.rs`.
        let cmd_rig_conn_state = self.rig_conn_state.clone();
        let cmd_audio_output_alive = self.audio_output_alive.clone();
        // Operator Hold/Auto TX-frequency mode (`f`). The handler toggles this
        // atomic; the QSO engine and autonomous operator read it to gate
        // autonomous frequency moves.
        let cmd_tx_freq_mode = self.tx_freq_mode.clone();
        // PAN-39: bumped alongside every `cmd_tx_freq_mode.store()` below so
        // the autonomous operator can detect a transition it didn't directly
        // observe (see `AutonomousOperator::set_tx_freq_mode_generation_source`).
        let cmd_tx_freq_mode_generation = self.tx_freq_mode_generation.clone();
        // PAN-143: shared with QsoManager's apply_tx_offset_switch and
        // PttGuard::new so every `cmd_tx_freq_mode.store()` below is
        // genuinely atomic against that method's recheck of the same
        // atomic, not just narrowly raced against it.
        let cmd_ptt_sync_gate = self.ptt_sync_gate.clone();
        // Held TX audio offset in Hz (0 = Auto/unset). Written by the `o`-modal
        // relay arm; read by the manual-call handler at QSO open to place our
        // TX audio offset when the operator has set one.
        let cmd_tx_offset_hold_hz = self.tx_offset_hold_hz();
        // decoder-speed-overhaul Task 15: live decode-effort cycling (`e`).
        // The handler cycles `cmd_current_decode_effort`, resolves `Auto`
        // against `cmd_resolved_hardware_tier`, and publishes the resulting
        // budget with its decoder config into `cmd_decode_window` — the one
        // `DecodeWindowConfig` `ft8.rs` reads once per window (PAN-164).
        let cmd_current_decode_effort = self.current_decode_effort();
        let cmd_resolved_hardware_tier = self.resolved_hardware_tier();
        // Mode-switch machinery (Shift+M). Cloned here (not previously
        // needed by this task) so the CycleOperatingMode handler can call
        // `try_switch_operating_mode` directly, mirroring how `cmd_tx_policy`
        // is used by CycleTxPolicy. `decode_window`/`active_tx_qsos` are
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
        // fallback when no QSO is active) — this relay never MUTATES
        // `QsoManager` itself, matching the established AbortQso/ResendQso
        // pattern of forwarding rather than mutating QsoManager directly from
        // this task.
        //
        // Round 2 finding 4 does add a READ-ONLY handle (via the same
        // restart-safe watch channel the Autonomous task's drain re-borrows
        // every tick) purely to ask which QSOs are genuinely non-terminal:
        // `active_tx_qsos` alone cannot answer that, since it retains a
        // completed QSO for its 45s trailing-73 grace window. Commits still
        // happen only in the Autonomous drain.
        let cmd_active_tx_offsets = self.active_tx_offsets.clone();
        let cmd_qso_manager_watch = self.qso_manager_watch.subscribe();
        let cmd_pending_qso_offset_requests = self.pending_qso_offset_requests.clone();
        let cmd_pending_cq_offset_nudge = self.pending_cq_offset_nudge.clone();
        let cmd_decode_window = self.decode_window.clone();
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
        // bookmark saves silently land in the wrong file. PAN-200 keeps
        // the as-given path here; each write resolves it on the worker.
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
        // PAN-60: single-flight flag for `spawn_rig_config_refresh` (startup + every `i` open).
        let cmd_rig_refresh_in_flight = Arc::new(std::sync::atomic::AtomicBool::new(false));
        // PAN-60 review: `SelectRig` commands handled so far, stamped on every
        // `RigConfigUpdate` so the TUI can tell a reply that predates its
        // latest submit.
        let cmd_rig_select_seq = Arc::new(std::sync::atomic::AtomicU64::new(0));
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
            // TUI (PAN-59), so the `i` rig picker has data from the start.
            // Opening the picker asks for a fresh copy
            // (`TuiCommand::RequestRigConfig`, PAN-60); both go through
            // `spawn_rig_config_refresh`, which enumerates on the blocking
            // pool rather than on this task.
            let _ = spawn_rig_config_refresh(
                cmd_config.clone(),
                cmd_tui_msg_tx.clone(),
                cmd_rig_refresh_in_flight.clone(),
                cmd_rig_select_seq.clone(),
                enumerate_serial_ports,
            );

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
                let budget_ms = cmd_decode_window.read().await.budget_ms();
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
                                    attempt_id: None,
                                    message_text: text,
                                    frequency_offset,
                                    qso_id: None,
                                    tx_parity: None, // TUI manual send: no DX context
                                    origin: crate::message_bus::TxOrigin::Local,
                                    remote_client_key_id: None,
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
                                    remote_client_key_id: None,
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
                                        remote_client_key_id: None,
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
                                if cmd_split_tx_hz.load(Ordering::Relaxed) != 0 {
                                    let store = super::forward_split_change(
                                        &cmd_message_bus,
                                        ComponentId::Tui,
                                        &cmd_split_tx_hz,
                                        false,
                                        0,
                                    )
                                    .await;
                                    // Push authoritative split state to the TUI chip.
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::SplitUpdate {
                                            tx_hz: store,
                                        },
                                    );
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
                            info!(target: "rig.split", "TUI SetSplit enabled={} tx={} Hz", enabled, tx_frequency);
                            let store = super::forward_split_change(
                                &cmd_message_bus,
                                ComponentId::Tui,
                                &cmd_split_tx_hz,
                                enabled,
                                tx_frequency,
                            )
                            .await;
                            // Push authoritative split state to the TUI chip.
                            let _ = cmd_tui_msg_tx.send(
                                pancetta_tui::tui_runner::TuiMessage::SplitUpdate { tx_hz: store },
                            );
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
                                    remote_client_key_id: None,
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
                                &cmd_decode_window,
                                &cmd_active_protocol_mode,
                                &cmd_active_slot_ns,
                                &cmd_active_decode_phase_ns,
                                &cmd_current_decode_effort,
                                &cmd_resolved_hardware_tier,
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
                            //
                            // PAN-164: the cycle, its tier read and its
                            // publish all happen under one `decode_window`
                            // write guard, so the hot loop sees the whole
                            // change (budget + the Ft8Config overrides
                            // derived from it) or none of it. Reading the
                            // tier under the lock also serialises this with
                            // the tier probe, which stores it inside its own
                            // critical section on this same lock (PAN-156
                            // rounds 2 and 6): whichever runs second
                            // publishes a consistent final state.
                            let (next, budget_ms, tier) = {
                                let mut window_guard = cmd_decode_window.write().await;
                                let tier = pancetta_ft8::tier_probe::HardwareTier::from_u8(
                                    cmd_resolved_hardware_tier.load(Ordering::Acquire),
                                );
                                let (next, budget_ms) = super::effort::cycle_decode_effort(
                                    &cmd_current_decode_effort,
                                    &mut window_guard,
                                    tier,
                                );
                                (next, budget_ms, tier)
                            };
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
                            // adaptive stall switch/revert + the `u` nudge).
                            // Update the shared atomic; the TUI
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
                            //
                            // PAN-143: also under `cmd_ptt_sync_gate` so this
                            // store is mutually exclusive with QsoManager's
                            // `apply_tx_offset_switch` recheck of the same
                            // atomic, closing (not just narrowing) that race.
                            {
                                let _ptt_sync = cmd_ptt_sync_gate.lock().await;
                                cmd_tx_freq_mode_generation.fetch_add(1, Ordering::SeqCst);
                                cmd_tx_freq_mode.store(next.as_u8(), Ordering::SeqCst);
                            }
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
                                    // PAN-143: also under `cmd_ptt_sync_gate`
                                    // -- see ToggleTxFreqMode's PAN-143 note.
                                    {
                                        let _ptt_sync = cmd_ptt_sync_gate.lock().await;
                                        cmd_tx_freq_mode_generation.fetch_add(1, Ordering::SeqCst);
                                        cmd_tx_freq_mode.store(
                                            pancetta_core::TxFreqMode::Hold.as_u8(),
                                            Ordering::SeqCst,
                                        );
                                    }
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
                                    // PAN-143: also under `cmd_ptt_sync_gate`
                                    // -- see ToggleTxFreqMode's PAN-143 note.
                                    {
                                        let _ptt_sync = cmd_ptt_sync_gate.lock().await;
                                        cmd_tx_freq_mode_generation.fetch_add(1, Ordering::SeqCst);
                                        cmd_tx_freq_mode.store(
                                            pancetta_core::TxFreqMode::Auto.as_u8(),
                                            Ordering::SeqCst,
                                        );
                                    }
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
                            // Round 2 finding 4: `active` still contains a
                            // completed QSO for its 45s trailing-73 grace
                            // window, so it cannot be trusted to answer
                            // "is there a QSO to nudge?". Ask the QSO
                            // engine itself. Clone the handle out of the
                            // watch `Ref` and drop it BEFORE the `.await`
                            // (same reason as the autonomous drain's own
                            // borrow), and re-borrow fresh each press so a
                            // Qso-component restart is picked up. `None`
                            // (component not up) preserves the pre-filter
                            // behavior -- see `resolve_nudge_tx_offset`.
                            // PAN-72 round-8 redesign (fix 3): derive
                            // `auto_calling_cq_ids` from the SAME
                            // `get_active_qsos()` result as `live_qso_ids` —
                            // no extra `QsoManager` call needed. These are
                            // the QSOs an ordinary `u` nudge must NOT route
                            // through `pending_qso_offset_requests`: an
                            // autonomous self-CQ's `CallingCq` retransmit is
                            // deliberately dormant for `CallInitiation::Auto`
                            // (`rearm_manual_calls_at` only re-keys Manual),
                            // so that path would silently update metadata
                            // with no frame ever re-sent. See
                            // `resolve_nudge_tx_offset`'s doc comment.
                            // PAN-72 round-9 fix (Codex round 9, finding 2):
                            // `rearm_eligible_ids` derived from the SAME
                            // `get_active_qsos()` result as the two sets
                            // above -- the live QSOs whose CURRENT state
                            // `QsoManager::rearm_manual_calls_at` will
                            // actually retransmit a frame for. See
                            // `qso_state_is_rearm_eligible` and
                            // `resolve_nudge_tx_offset`'s doc comment.
                            let (live_qso_ids, auto_calling_cq_ids, rearm_eligible_ids) = {
                                let manager = cmd_qso_manager_watch.borrow().clone();
                                match manager {
                                    Some(m) => {
                                        let active_qsos = m.get_active_qsos().await;
                                        let live: std::collections::HashSet<_> =
                                            active_qsos.iter().map(|(id, _)| *id).collect();
                                        let auto_calling_cq: std::collections::HashSet<_> =
                                            active_qsos
                                                .iter()
                                                .filter(|(_, progress)| {
                                                    progress.metadata.initiated_by
                                                        == pancetta_qso::states::CallInitiation::Auto
                                                        && matches!(
                                                            progress.state,
                                                            pancetta_qso::states::QsoState::CallingCq { .. }
                                                        )
                                                })
                                                .map(|(id, _)| *id)
                                                .collect();
                                        let rearm_eligible: std::collections::HashSet<_> =
                                            active_qsos
                                                .iter()
                                                .filter(|(_, progress)| {
                                                    qso_state_is_rearm_eligible(progress)
                                                })
                                                .map(|(id, _)| *id)
                                                .collect();
                                        (Some(live), Some(auto_calling_cq), Some(rearm_eligible))
                                    }
                                    None => (None, None, None),
                                }
                            };
                            let switched = resolve_nudge_tx_offset(
                                &cmd_tx_freq_mode,
                                &active,
                                live_qso_ids.as_ref(),
                                auto_calling_cq_ids.as_ref(),
                                rearm_eligible_ids.as_ref(),
                                &cmd_active_tx_offsets,
                                &cmd_pending_qso_offset_requests,
                                &cmd_pending_cq_offset_nudge,
                            );
                            match switched {
                                NudgeOutcome::ActiveQso(qso_id) => {
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
                                NudgeOutcome::CqNudgeArmed => {
                                    info!(
                                        target: "tx.freq",
                                        "TUI NudgeTxOffset: no active QSO — armed a CQ-offset nudge \
                                         (takes effect only if the next autonomous cycle is CQ-hunting)"
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status:
                                                "CQ-offset nudge armed — takes effect only if the \
                                                      next autonomous cycle is CQ-hunting"
                                                    .to_string(),
                                        },
                                    );
                                }
                                NudgeOutcome::HeldNoOp => {
                                    // Hold mode, with or without an active QSO:
                                    // for a QSO the downstream allocator would
                                    // ignore `avoid_hz` and hand back the parked
                                    // offset (a no-op at best, an unwanted move
                                    // onto the park offset at worst); for the CQ
                                    // fallback `decide_at`'s `tx_freq_auto()`
                                    // gate consumes and discards the flag. Say so
                                    // instead of claiming a nudge happened.
                                    info!(
                                        target: "tx.freq",
                                        "TUI NudgeTxOffset ignored: TX-frequency mode is Hold"
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: "TX offset is Hold — press `f` for Auto to \
                                                     enable nudging"
                                                .to_string(),
                                        },
                                    );
                                }
                                NudgeOutcome::NoEligibleTarget => {
                                    // PAN-72 round-9 fix (Codex round 9,
                                    // finding 2): a live QSO exists but its
                                    // current state cannot retransmit a
                                    // frame -- say so instead of claiming a
                                    // nudge happened.
                                    info!(
                                        target: "tx.freq",
                                        "TUI NudgeTxOffset ignored: the active QSO's current \
                                         state cannot retransmit a frame"
                                    );
                                    let _ = cmd_tui_msg_tx.send(
                                        pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                                            component: "TX".to_string(),
                                            status: "Nudge ignored — the active QSO's current \
                                                     state can't retransmit a frame"
                                                .to_string(),
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
                                    &cmd_rig_conn_state,
                                    &cmd_audio_output_alive,
                                    &cmd_message_bus.receive_gain,
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
                                // PAN-118: frequency-aware sibling of the gate
                                // above — a manual PTT key-up is a raw carrier on
                                // the effective TX dial, so refuse it while that
                                // dial sits outside the configured amateur band
                                // plan. Same status channel as every other PTT
                                // refusal; PTT-off remains always allowed below.
                                if let Some(status) = ptt_band_plan_refusal(
                                    &cmd_operating_freq_hz,
                                    &cmd_split_tx_hz,
                                    &cmd_band_plan,
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
                            if super::tx::send_ptt_command(
                                &cmd_message_bus,
                                ComponentId::Tui,
                                new_state,
                                || true,
                            ) != crate::message_bus::SendIfOutcome::Enqueued
                            {
                                warn!("Failed to toggle PTT: message dropped");
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
                                        let target = resolve_config_write_target(&write_path)?;
                                        pancetta_config::Config::default()
                                            .set_audio_devices_in_file(
                                                &target,
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
                                // Config is unchanged, so no lock is needed
                                // to order this against a refresh's read.
                                cmd_rig_select_seq.fetch_add(1, Ordering::AcqRel);
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
                                    // Under the write lock: a refresh reads
                                    // the count and the rig under its read
                                    // lock, so it sees both or neither.
                                    cmd_rig_select_seq.fetch_add(1, Ordering::AcqRel);
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
                                                let target =
                                                    resolve_config_write_target(&write_path)?;
                                                pancetta_config::Config::default().set_rig_in_file(
                                                    &target,
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
                        pancetta_tui::tui_runner::TuiCommand::RequestRigConfig => {
                            // PAN-60: the `i` picker opened. Hand the blocking
                            // enumeration to `spawn_rig_config_refresh` and
                            // return to `try_recv` at once -- the same I4 rule
                            // as SelectRig above.
                            if spawn_rig_config_refresh(
                                cmd_config.clone(),
                                cmd_tui_msg_tx.clone(),
                                cmd_rig_refresh_in_flight.clone(),
                                cmd_rig_select_seq.clone(),
                                enumerate_serial_ports,
                            )
                            .is_none()
                            {
                                debug!(
                                    "RequestRigConfig: a refresh is already in flight; its reply answers this request"
                                );
                            }
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

        let dx_need_lookup = self.cached_lookup.clone();

        // Start TUI runner in a blocking task so it can own the terminal
        let tui_handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            rt.block_on(async {
                pancetta_tui::tui_runner::run_tui_with_message_bus_and_need(
                    tui_config,
                    tui_msg_rx,
                    tui_cmd_tx,
                    shutdown,
                    tui_last_input,
                    Some(dx_need_lookup),
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
        pending_freq_drift_hz: q.pending_freq_drift_hz,
        pending_freq_drift_since: q.pending_freq_drift_since,
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
/// Auto-gating (PAN-72 final review, finding 2): the active-QSO branch is
/// gated on `TxFreqMode::allows_auto_change()`, exactly like the CQ-hunting
/// branch it falls back to (`AutonomousOperator`'s `should_switch =
/// self.tx_freq_auto() && (streak_hit || manual_switch_requested)`). Without
/// the gate, a Hold-mode nudge queues a `Switch` whose eventual
/// `allocate_smart_frequency` call takes the Hold early-return — which
/// ignores `avoid_hz` entirely and yields the *parked* offset
/// (`tx_offset_hold_hz`, else `config.tx_offset_hz`). That either does
/// nothing visible or drags an intentionally-held QSO onto the park offset,
/// while the status line claims a real nudge happened; both violate Hold's
/// stickiness invariant.
///
/// In Hold with an active QSO the CQ fallback is deliberately NOT armed
/// either: there *is* an active QSO, so the CQ-hunting path is simply the
/// wrong target, and arming it would be a second silent false-success.
///
/// Hold gating covers the CQ fallback too (Codex round 1 on PR #350, finding
/// 4): the mode check is hoisted ABOVE the active-QSO branch so a Hold-mode
/// `u` with no active QSO also reports [`NudgeOutcome::HeldNoOp`] instead of
/// claiming a CQ nudge. `decide_at` computes `should_switch =
/// self.tx_freq_auto() && (...)`, so a flag armed in Hold is consumed and
/// discarded with nothing to show for it — a guaranteed silent no-op that the
/// status line used to report as a successful nudge.
///
/// The *second* half of that finding — "in Auto but not currently CQ-hunting,
/// `decide_at` also consumes and discards the request" — is NOT gated here,
/// deliberately. Nothing in the coordinator or this relay task has any view of
/// `AutonomousOperator::state`: it is a private field of an operator owned
/// exclusively by the autonomous task, and unlike `active_tx_qsos` /
/// `active_tx_offsets` / `tx_freq_mode` there is no shared snapshot or atomic
/// carrying it. Adding one would be new cross-task state-visibility plumbing
/// for a check that still could not be authoritative: `decide_at` only enters
/// its CQ branch after re-evaluating `idle_cycles >=
/// cq_after_idle_cycles` for THAT cycle, so any state this relay could read at
/// keypress time is a prediction, not a guarantee. Worse, `decide_at`
/// unconditionally `mem::take`s `manual_switch_requested` at the very top of
/// every call, so the flag survives exactly one `decide_at` invocation and
/// only actually fires if THAT SPECIFIC call reaches the CQ-hunting branch —
/// if it lands on `NotOurSlot`/`Listen`/paused/anything else instead, the
/// request is silently discarded, not deferred to a later cycle. Instead the
/// returned outcome — and the operator-facing status line — is worded as an
/// *armed request that takes effect only if the very next autonomous cycle
/// happens to be CQ-hunting*, which is exactly what it is. (Finding 9's fix
/// additionally stops such a request being lost when that cycle arrives with
/// thin decode history.)
///
/// Terminal-QSO filtering (Codex round 2 on PR #350, finding 4): `active` is
/// the coordinator's `active_tx_qsos` snapshot, which deliberately RETAINS a
/// completed QSO for 45 seconds so its trailing 73 can still transmit. Picking
/// out of it blind means the `u` key can name a QSO that is already terminal —
/// either the only entry, or one chosen ahead of a genuinely live concurrent
/// QSO — and the operator is told "Nudging active QSO" while the drain's
/// `apply_tx_offset_switch` later refuses it with `QsoNotActive` and nothing
/// moves. `live_qso_ids` is the authoritative set (from
/// `QsoManager::get_active_qsos`, i.e. `QsoState::is_active`) that the caller
/// reads at keypress time; only keys present in it are eligible, so a
/// grace-window-only snapshot correctly falls through to the CQ fallback.
///
/// `None` means the caller could not consult a `QsoManager` at all (the Qso
/// component is not up), which is exactly the situation where nothing could
/// commit anyway — it preserves the pre-filter behavior rather than inventing
/// a different one for a state the operator cannot act in.
///
/// This is deliberately a SELECTION-time filter, complementing (not replacing)
/// the drain's commit-time refusal: the commit-time check is what keeps the
/// engine correct, this one is what keeps the status line honest.
///
/// PAN-72 round-8 redesign (fix 3, Codex round 8 finding 3), SUPERSEDED by
/// round 10's Fix A below: an autonomous self-CQ (`CallInitiation::Auto`,
/// `QsoState::CallingCq`) is live and present in `active`/`live_qso_ids`, so
/// without `auto_calling_cq_ids` it would be picked as an ordinary
/// active-QSO target and routed through `pending_qso_offset_requests` (the
/// `operator_forced` `Switch` path). But `QsoManager::rearm_manual_calls_at`
/// only retransmits `CallingCq` for `CallInitiation::Manual` — Auto stays
/// deliberately dormant there (a separate, intentional design decision this
/// fix does NOT touch) — so that request used to silently update metadata
/// with no frame ever re-sent, while `AutonomousOperator::current_cq_offset_
/// hz` stayed on the old offset and got reused by the next autonomous CQ
/// attempt: the `u` key did nothing visible for this state, while the status
/// line and drain both reported success.
///
/// Round 8's fix routed AROUND that gate by falling through to
/// `pending_cq_offset_nudge` instead — the same one-shot flag the "no active
/// QSO" fallback below sets. Round 10's Fix A (Codex round 10, thread on
/// `autonomous.rs:1829`) found that mechanism structurally could never fire
/// for the exact case it existed for: `AutonomousOperator::decide_at` only
/// consumes `manual_switch_requested` from its CQ-hunting branch, gated on
/// `active_qso_count == 0` — and the `CallingCq` being nudged IS that one
/// active QSO, so the count is never 0 while it's open. The flag was
/// silently discarded by `decide_at`'s unconditional `mem::take` every time.
///
/// Fix A routes an auto-CQ target through `pending_qso_offset_requests`
/// instead — the SAME path an ordinary target uses — relying on
/// `QsoManager::apply_tx_offset_switch` (round 10) to supply the missing
/// piece: when it commits an `OperatorForced` switch for a `CallingCq` QSO,
/// it now ALSO emits a one-shot CQ retransmission at the new offset,
/// independent of (and without touching) `rearm_manual_calls_at`'s own
/// Manual-only periodic cadence. This is a narrow, project-owner-approved
/// exception for a deliberate, one-shot, operator-supervised action — NOT a
/// loosening of the protected `is_manual` gate, which stays exactly as it
/// was for unsupervised, cadence-driven keep-calling.
///
/// PAN-72 round-9 fix (Codex round 9, finding 2): does `progress`'s CURRENT
/// state belong to the exact set `QsoManager::rearm_manual_calls_at`
/// actually retransmits a frame for? Mirrors that function's `match
/// &progress.state` block (`pancetta-qso/src/qso_manager.rs`) — including
/// its Manual/Auto conditioning for `CallingCq`/`WaitingForReport` — rather
/// than re-deriving the list independently, so the two can never drift
/// apart silently.
///
/// `RespondingToCq` and `SendingReport` are eligible for EITHER initiation
/// (`rearm_manual_calls_at` re-sends an autonomous pounce's call/report
/// too, per SM-F6); `CallingCq` and `WaitingForReport` are Manual-only in
/// THIS predicate — `rearm_manual_calls_at`'s own periodic cadence still
/// never re-sends an Auto `CallingCq`/`WaitingForReport`, unchanged. An Auto
/// `CallingCq` is still selectable as a nudge target, just through the
/// separate `auto_calling_cq_ids` set (Fix A, above), not through this
/// predicate. Every other state (`WaitingForConfirmation`,
/// `SendingConfirmation`, a contest-exchange state, `Idle`, terminal states,
/// etc.) falls to `rearm_manual_calls_at`'s own `_ => continue` and is
/// therefore NOT eligible here either.
///
/// PAN-131: this predicate together with `auto_calling_cq_ids` must equal
/// `pancetta_qso::operator_nudge_retransmits`, the same check
/// `apply_tx_offset_switch` re-runs at commit time (its guard 2a).
fn qso_state_is_rearm_eligible(progress: &pancetta_qso::QsoProgress) -> bool {
    let is_manual = progress.metadata.initiated_by == pancetta_qso::states::CallInitiation::Manual;
    match &progress.state {
        pancetta_qso::states::QsoState::CallingCq { .. } => is_manual,
        pancetta_qso::states::QsoState::WaitingForReport { .. } => is_manual,
        pancetta_qso::states::QsoState::RespondingToCq { .. } => true,
        pancetta_qso::states::QsoState::SendingReport { .. } => true,
        _ => false,
    }
}

/// Returns [`NudgeOutcome`] for the caller's status echo/logging.
#[allow(clippy::too_many_arguments)]
fn resolve_nudge_tx_offset(
    tx_freq_mode: &std::sync::atomic::AtomicU8,
    active: &std::collections::HashSet<String>,
    live_qso_ids: Option<&std::collections::HashSet<pancetta_qso::QsoId>>,
    auto_calling_cq_ids: Option<&std::collections::HashSet<pancetta_qso::QsoId>>,
    rearm_eligible_ids: Option<&std::collections::HashSet<pancetta_qso::QsoId>>,
    active_tx_offsets: &std::sync::RwLock<std::collections::HashMap<String, f64>>,
    pending_qso_offset_requests: &std::sync::Mutex<
        Vec<pancetta_qso::qso_manager::OffsetActionRequest>,
    >,
    pending_cq_offset_nudge: &std::sync::atomic::AtomicBool,
) -> NudgeOutcome {
    if !pancetta_core::TxFreqMode::from_u8(tx_freq_mode.load(Ordering::Acquire))
        .allows_auto_change()
    {
        return NudgeOutcome::HeldNoOp;
    }
    // PAN-72 Fix D (Codex round 10, thread on `tui_relay.rs:3134`): fold
    // eligibility (round-9's rearm-eligible check, and round-8's auto-CQ
    // check) into the SAME predicate the search uses for liveness, so it
    // naturally continues past an ineligible live QSO to one that
    // qualifies. The OLD code did `.find()` on liveness ALONE, then checked
    // eligibility only on whatever arbitrary (`HashSet` iteration order)
    // first live match came back — with two concurrent QSOs, one
    // live-but-ineligible and one live-and-eligible, that could return
    // `NoEligibleTarget` even though a perfectly good target existed,
    // purely based on which key the set happened to visit first.
    let target = active.iter().find(|key| {
        key.parse::<pancetta_qso::QsoId>().is_ok_and(|id| {
            live_qso_ids.is_none_or(|live| live.contains(&id))
                && (rearm_eligible_ids.is_none_or(|ids| ids.contains(&id))
                    || auto_calling_cq_ids.is_some_and(|ids| ids.contains(&id)))
        })
    });
    if let Some(key) = target {
        if let Ok(qso_id) = key.parse::<pancetta_qso::QsoId>() {
            // PAN-72 Fix A (Codex round 10, thread on `autonomous.rs:1829`):
            // an autonomous self-CQ (`auto_calling_cq_ids`) is routed through
            // this SAME `pending_qso_offset_requests` path now too, exactly
            // like any other rearm-eligible target — see this function's doc
            // comment for why round 8's CQ-nudge-fallback routing could never
            // actually fire for the case it existed for, and how
            // `QsoManager::apply_tx_offset_switch` now supplies the missing
            // one-shot retransmission `rearm_manual_calls_at`'s Manual-only
            // cadence deliberately never does for an Auto `CallingCq`. The
            // combined predicate above already guarantees this branch is
            // reached only when `rearm_eligible_ids` or `auto_calling_cq_ids`
            // says so (or `rearm_eligible_ids` is `None`, preserving the
            // pre-round-9 behavior) — no separate eligibility re-check
            // needed here.
            let current = active_tx_offsets
                .read()
                .ok()
                .and_then(|m| m.get(key).copied())
                .unwrap_or(1500.0);
            if let Ok(mut pending) = pending_qso_offset_requests.lock() {
                // Operator-forced: no staleness token (PAN-72 finding 8).
                // The operator pressed `u` for the QSO as it stands right
                // now, so an ordinary advance does not supersede it — but
                // `apply_tx_offset_switch`'s guard 2a refuses it if the QSO
                // has left the nudge-eligible states by commit time (PAN-131).
                pending.push(
                    pancetta_qso::qso_manager::OffsetActionRequest::operator_forced(
                        qso_id,
                        pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz: current },
                    ),
                );
            }
            return NudgeOutcome::ActiveQso(qso_id);
        }
    }
    // PAN-72 round-9 fix (Codex round 9, finding 2), preserved by Fix D:
    // the combined predicate above conflates "no live QSO at all" with
    // "live QSOs exist but none are eligible" — both simply fail to match.
    // Check `live_qso_ids` alone, separately, to tell them apart: only the
    // former may fall through to the CQ-hunting fallback below.
    let any_live = active.iter().any(|key| {
        key.parse::<pancetta_qso::QsoId>()
            .is_ok_and(|id| live_qso_ids.is_none_or(|live| live.contains(&id)))
    });
    if any_live {
        // Deliberately NOT falling through to the CQ-nudge fallback: that
        // flag is for CQ-hunting, and arming it here would silently consume
        // `pending_cq_offset_nudge` against an unrelated future CQ-hunting
        // cycle while implying something was armed for THIS QSO.
        return NudgeOutcome::NoEligibleTarget;
    }
    pending_cq_offset_nudge.store(true, Ordering::Relaxed);
    NudgeOutcome::CqNudgeArmed
}

/// What one `TuiCommand::NudgeTxOffset` dispatch actually did — see
/// [`resolve_nudge_tx_offset`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NudgeOutcome {
    /// Forced a `Switch` onto `pending_qso_offset_requests` for this QSO.
    /// The autonomous drain commits it on the next slot tick.
    ActiveQso(pancetta_qso::states::QsoId),
    /// No active QSO — ARMED the one-shot CQ-offset nudge flag. Deliberately
    /// not called "CqNudge": whether it produces a real offset move depends
    /// on the autonomous operator's very next `decide_at` call happening to
    /// reach its CQ-hunting branch — the flag is consumed unconditionally on
    /// that next call regardless of which branch it reaches, so it does NOT
    /// wait around for a later CQ cycle, which nothing here can see or
    /// promise (see `resolve_nudge_tx_offset`'s doc comment). The status line
    /// says "armed", not "nudged".
    CqNudgeArmed,
    /// TX-frequency mode is Hold: nothing queued, nothing armed, for an
    /// active QSO or the CQ fallback alike. A flag armed in Hold is consumed
    /// and discarded by `decide_at`'s `tx_freq_auto()` gate, so claiming a
    /// nudge would be a silent false success. The operator is told to press
    /// `f` for Auto.
    HeldNoOp,
    /// PAN-72 round-9 fix (Codex round 9, finding 2): there IS a live QSO,
    /// but its CURRENT state is not one `QsoManager::rearm_manual_calls_at`
    /// will ever retransmit a frame for (e.g. `WaitingForConfirmation`) —
    /// nothing queued, nothing armed. The old code queued a `Switch` anyway,
    /// silently updating offset metadata while reporting success with
    /// nothing ever transmitted at the new offset. Deliberately does NOT
    /// fall through to the CQ-nudge fallback either — that flag is for
    /// CQ-hunting, and arming it here would silently consume it against an
    /// unrelated future CQ-hunting cycle while implying something was armed
    /// for this QSO. The status line explains why, mirroring `HeldNoOp`.
    NoEligibleTarget,
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
/// `dsp.rs`'s `last_known_dial_hz`/`cur_dial_hz`) — falls back to the same
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
/// `CachedStationLookup` the autonomous scorer consults. `needed` only when
/// cqdx's loaded list names the entity; false while the list is unknown
/// (PAN-93), or the callsign is absent. `atno` implies `needed`.
fn needed_atno_for(
    lookup: &crate::priority_evaluator::CachedStationLookup,
    callsign: Option<&str>,
) -> (bool, bool) {
    match callsign {
        Some(c) if !c.is_empty() => lookup.display_needed_atno(c),
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

/// The station's known US state, from the same `CachedStationLookup` the
/// scorer uses. `None` when unknown — the TUI then renders the entity
/// alone (PAN-85).
fn state_for(
    lookup: &crate::priority_evaluator::CachedStationLookup,
    callsign: Option<&str>,
) -> Option<String> {
    callsign
        .filter(|c| !c.is_empty())
        .and_then(|c| lookup.state_for(c))
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
#[allow(clippy::too_many_arguments)]
fn ptt_on_refusal(
    tx_policy: &Arc<std::sync::atomic::AtomicU8>,
    tx_restart_inhibit: &Arc<std::sync::atomic::AtomicU32>,
    hamlib_command_loop_ready: &Arc<std::sync::atomic::AtomicBool>,
    hamlib_pending_frequency: &Arc<
        std::sync::Mutex<std::collections::HashMap<pancetta_hamlib::Vfo, ComponentMessage>>,
    >,
    hamlib_pending_split: &Arc<std::sync::Mutex<Option<ComponentMessage>>>,
    hamlib_command_in_flight: &Arc<std::sync::atomic::AtomicU32>,
    rig_conn_state: &Arc<std::sync::atomic::AtomicU8>,
    audio_output_alive: &Arc<std::sync::atomic::AtomicBool>,
    receive_gain: &Arc<super::receive_overload::GainSafety>,
) -> Option<String> {
    super::tx::tx_hard_mute_reason(
        tx_policy,
        tx_restart_inhibit,
        hamlib_command_loop_ready,
        hamlib_pending_frequency,
        hamlib_pending_split,
        hamlib_command_in_flight,
        rig_conn_state,
        audio_output_alive,
        receive_gain,
    )
    .map(|reason| format!("Can't key PTT — {reason}"))
}

/// PAN-118: the frequency-aware sibling of `ptt_on_refusal` — a manual PTT
/// key-up (Shift+T) is a transmission too: a raw carrier at the effective
/// TX dial (split when active, else RX dial), with no audio offset. Refuse
/// it while that dial sits outside the configured amateur band plan
/// (`edge_warnings` is the master switch, so `false` remains the
/// operator's escape hatch). `None` means the key-up may proceed;
/// `Some(status)` is the operator-facing status text. The dial-vs-split
/// resolution and the out-of-band boolean come from the TX worker's shared
/// pure helpers (`tx::tx_rf_for_gate` / `tx::tx_band_plan_mute_reason`),
/// so the split-vs-RX-dial rule and the band tables have exactly one
/// implementation each. Skips the check while the dial reads the
/// unknown-0 sentinel (mock/rig-disabled dev modes), mirroring the worker.
fn ptt_band_plan_refusal(
    operating_freq_hz: &Arc<std::sync::atomic::AtomicU64>,
    split_tx_hz: &Arc<std::sync::atomic::AtomicU64>,
    band_plan: &pancetta_config::BandPlanConfig,
) -> Option<String> {
    // A raw carrier keys AT the dial — offset 0.0.
    super::tx::tx_rf_for_gate(operating_freq_hz, split_tx_hz, 0.0).and_then(|rf| {
        if super::tx::tx_band_plan_mute_reason(rf, 0.0, band_plan).is_some() {
            Some(format!(
                "Can't key PTT — TX dial {rf:.0} Hz is outside the configured amateur band plan"
            ))
        } else {
            None
        }
    })
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

/// PAN-200: resolve the file a targeted write replaces when the write runs
/// on the blocking worker, so a repointed `--config` symlink gets the save.
/// Atomic replacement must land on the real target, never on the symlink
/// (PAN-62), so a dangling symlink refuses the write. A missing plain path
/// (first-run default) is written as given.
fn resolve_config_write_target(
    config_path: &std::path::Path,
) -> pancetta_config::ConfigResult<std::path::PathBuf> {
    match std::fs::canonicalize(config_path) {
        Ok(target) => Ok(target),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            match std::fs::symlink_metadata(config_path) {
                Ok(meta) if meta.file_type().is_symlink() => {
                    Err(pancetta_config::ConfigError::Validation(format!(
                        "{} is a symlink to a missing file",
                        config_path.display()
                    )))
                }
                _ => Ok(config_path.to_path_buf()),
            }
        }
        Err(e) => Err(e.into()),
    }
}

/// PAN-200 review (Codex P2): the `[rig].bookmarks` list the config file at
/// `path` holds now, or `None` when the file or the key is missing. A
/// bookmark write applies its mutation to this list, so a save after a
/// repoint keeps the new target's own bookmarks instead of replacing them
/// with the running (pre-repoint) list.
fn read_rig_bookmarks_in_file(
    path: &std::path::Path,
) -> pancetta_config::ConfigResult<Option<Vec<pancetta_config::rig::RigBookmark>>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let doc: toml::Table = toml::from_str(&contents).map_err(|e| {
        pancetta_config::ConfigError::Validation(format!("Failed to parse config: {}", e))
    })?;
    let Some(bookmarks) = doc.get("rig").and_then(|rig| rig.get("bookmarks")) else {
        return Ok(None);
    };
    bookmarks.clone().try_into().map(Some).map_err(|e| {
        pancetta_config::ConfigError::Validation(format!("Failed to parse rig bookmarks: {}", e))
    })
}

/// One targeted config-file write (PAN-62: the coordinator's `config_path`,
/// not always `~/.pancetta/pancetta.toml`), queued for serialized
/// processing by the worker `spawn_config_file_write_worker` spawns
/// (PAN-61 review round 7, P1). `work` performs the actual blocking
/// `std::fs` I/O, including write-time target resolution (PAN-200), and is only
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
#[derive(Clone)]
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
            let (written_tx, written_rx) = tokio::sync::oneshot::channel();
            let write_path = config_path.clone();
            let write_staged = staged.clone();
            let write_mutation = mutation.clone();
            if write_tx
                .send(ConfigFileWriteRequest {
                    work: Box::new(move || {
                        let target = resolve_config_write_target(&write_path)?;
                        // PAN-200 review (Codex P2): apply the mutation to
                        // the list the target holds now. After a repoint
                        // that is not the running list; replacing it would
                        // drop the new target's own bookmarks. A target with
                        // no list keeps the running one, which a lower
                        // config layer may supply (PAN-63).
                        let written = match read_rig_bookmarks_in_file(&target)? {
                            Some(mut on_disk) => {
                                match write_mutation {
                                    BookmarkMutation::Save(bookmark) => {
                                        upsert_rig_bookmark(&mut on_disk, bookmark)
                                    }
                                    BookmarkMutation::Delete(name) => {
                                        on_disk.retain(|b| b.name != name)
                                    }
                                }
                                on_disk
                            }
                            None => write_staged,
                        };
                        pancetta_config::Config::default()
                            .set_rig_bookmarks_in_file(&target, &written)?;
                        let _ = written_tx.send(written);
                        Ok(())
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
                // The list the write job persisted, which the in-memory
                // state and the TUI now mirror (PAN-200 review).
                if let Ok(written) = written_rx.await {
                    staged = written;
                }
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

/// Serial port names for the `i` rig picker (PAN-60). Blocking -- udev or
/// sysfs on Linux, SetupAPI plus the SERIALCOMM registry key on Windows,
/// IOKit on macOS -- so it only ever runs through
/// `spawn_rig_config_refresh`, on tokio's blocking pool. A failed
/// enumeration is logged and yields an empty list; the TUI still offers
/// the configured port (`App::apply_rig_config_update`'s I-2a prepend).
fn enumerate_serial_ports() -> Vec<String> {
    match serialport::available_ports() {
        Ok(ports) => ports.into_iter().map(|p| p.port_name).collect(),
        Err(e) => {
            warn!("Serial port enumeration failed: {}", e);
            Vec::new()
        }
    }
}

/// Clears `spawn_rig_config_refresh`'s single-flight flag when the refresh
/// task ends -- normally, on an enumerator panic, or when the runtime drops
/// the task at shutdown.
struct RigConfigRefreshInFlight(Arc<std::sync::atomic::AtomicBool>);

impl Drop for RigConfigRefreshInFlight {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

/// Re-enumerate serial ports and push a fresh `TuiMessage::RigConfigUpdate`
/// to the TUI (PAN-60). Runs once at startup and for every
/// `TuiCommand::RequestRigConfig` (the `i` picker opening).
///
/// Never waits on its caller's task: the relay loop that calls it also
/// carries OperatorEmergencyStop/StopTx/TogglePtt/AbortQso (the I4 rule on
/// `SelectRig`'s arm), so the enumeration runs on the blocking pool inside
/// a detached task and this returns at once.
///
/// Single-flight: returns `None` without spawning while a refresh is
/// already running; that refresh's reply answers the new request too. At
/// most one blocking-pool thread is ever parked here, even if a serial
/// driver stalls the enumeration.
///
/// Config is read AFTER enumerating, so a rig switch applied while the
/// enumeration ran is in the reply. The read guard is held across the
/// send so the reply is ordered against the bookmark worker's commit and
/// its `RigBookmarksUpdate` -- a stale bookmark list can never land last.
/// The send is `try_send`: a full TUI channel drops this reply (the next
/// open asks again) rather than parking a runtime thread under that lock.
///
/// The reply carries `select_seq`, the number of `SelectRig` commands
/// handled so far, read under the same guard; `SelectRig` bumps it under
/// its config write lock, so the count always matches the rig values read
/// (PAN-60 review).
fn spawn_rig_config_refresh<E>(
    config: Arc<tokio::sync::RwLock<pancetta_config::Config>>,
    tui_msg_tx: crossbeam_channel::Sender<pancetta_tui::tui_runner::TuiMessage>,
    in_flight: Arc<std::sync::atomic::AtomicBool>,
    select_seq: Arc<std::sync::atomic::AtomicU64>,
    enumerate: E,
) -> Option<tokio::task::JoinHandle<()>>
where
    E: FnOnce() -> Vec<String> + Send + 'static,
{
    if in_flight.swap(true, Ordering::AcqRel) {
        return None;
    }
    let in_flight = RigConfigRefreshInFlight(in_flight);
    Some(tokio::spawn(async move {
        let _in_flight = in_flight;
        let available_ports = match tokio::task::spawn_blocking(enumerate).await {
            Ok(ports) => ports,
            Err(e) => {
                warn!("Serial port enumeration task failed: {}", e);
                Vec::new()
            }
        };
        let cfg = config.read().await;
        if let Err(e) = tui_msg_tx.try_send(pancetta_tui::tui_runner::TuiMessage::RigConfigUpdate {
            available_ports,
            current_model: cfg.rig.model.clone(),
            current_port: cfg.rig.interface.port.clone(),
            current_baud_rate: cfg.rig.interface.baud_rate,
            current_ptt_method: cfg.rig.ptt.method.clone(),
            bookmarks: cfg.rig.effective_bookmarks().to_vec(),
            select_seq: select_seq.load(Ordering::Acquire),
        }) {
            debug!("Failed to send rig config refresh to TUI: {}", e);
        }
    }))
}

#[cfg(test)]
mod tui_relay_tests {
    #[test]
    fn needed_atno_for_reports_unknown_as_not_needed() {
        // PAN-93: no cqdx list loaded → not shown as needed, even though the
        // scorer's no-exclusions guess would call every station needed.
        let lookup = crate::priority_evaluator::CachedStationLookup::new();
        assert_eq!(
            super::needed_atno_for(&lookup, Some("JA1ABC")),
            (false, false)
        );
        lookup.update_needed_dxcc(std::collections::HashSet::from(["JA".to_owned()]));
        lookup.update_needed_atno(std::collections::HashSet::from(["JA".to_owned()]));
        assert_eq!(
            super::needed_atno_for(&lookup, Some("JA1ABC")),
            (true, true)
        );
        assert_eq!(
            super::needed_atno_for(&lookup, Some("DL1ABC")),
            (false, false)
        );
        assert_eq!(super::needed_atno_for(&lookup, None), (false, false));
    }

    #[tokio::test]
    async fn gridless_decodes_keep_grid_rank_but_refresh_live_history() {
        use pancetta_qso::priority::WorkedStationLookup;
        use pancetta_tui::{app::App, Config, DecodedMessageView};

        let lookup = crate::priority_evaluator::CachedStationLookup::new();
        lookup.update_needed_dxcc(std::collections::HashSet::from(["DL".to_owned()]));
        // The entity is already worked, but FN42 is still new on this band.
        lookup.record_worked("W1XYZ", "20M");
        let scorer = pancetta_qso::PriorityScorer::new(
            pancetta_qso::priority::PriorityWeights::default(),
            Box::new(lookup.clone()),
        );
        let mut app = App::new(Config::default(), None).await.unwrap();
        let mut last_grids = super::RelayCache::default();
        for worked_grid in [false, true] {
            if worked_grid {
                lookup.record_worked_grid("FN42", "20M");
                lookup.record_failure("K1ABC");
            }
            let frames = if worked_grid {
                vec![("W1XYZ K1ABC -10", None)]
            } else {
                vec![
                    ("CQ K1ABC FN42", Some("FN42")),
                    ("W1XYZ K1ABC -10", None),
                    ("W1XYZ K1ABC R-10", None),
                    ("W1XYZ K1ABC RRR", None),
                    ("W1XYZ K1ABC RR73", None),
                    ("W1XYZ K1ABC 73", None),
                ]
            };
            for (text, grid) in frames {
                let grid = super::display_grid(&mut last_grids, Some("K1ABC"), grid, 14.074);
                let (priority_score, priority_factors) =
                    super::display_rank(&scorer, Some("K1ABC"), grid.as_deref(), -10.0, 14.074);
                app.add_decoded_message(DecodedMessageView {
                    timestamp: chrono::Utc::now(),
                    frequency: 14.074,
                    mode: "FT8".into(),
                    snr: -10,
                    delta_time: 0.0,
                    delta_freq: 1500.0,
                    call_sign: Some("K1ABC".into()),
                    grid_square: grid,
                    message: text.into(),
                    distance: None,
                    bearing: None,
                    slot_parity: None,
                    is_directed_at_us: false,
                    worked_before: lookup.is_duplicate("K1ABC", 14_074_000.0),
                    needed: lookup.is_needed_dxcc("K1ABC"),
                    atno: lookup.is_atno("K1ABC"),
                    band_needed: lookup.is_dxcc_needed_on_band("K1ABC", 14_074_000.0),
                    priority_score,
                    priority_factors,
                    is_own_tx: false,
                })
                .await
                .unwrap();
                let row = &app.dx_stations["K1ABC"];
                assert_eq!(row.grid_square.as_deref(), Some("FN42"));
                let factors = row.local_rank.unwrap().factors.unwrap();
                assert_eq!(factors.grid_new_on_band, !worked_grid, "{text}");
                assert_eq!(factors.recent_failure, worked_grid, "{text}");
                assert_eq!(
                    pancetta_qso::priority::PriorityTier::from_display_u32(row.priority_score),
                    if worked_grid {
                        pancetta_qso::priority::PriorityTier::Standard
                    } else {
                        pancetta_qso::priority::PriorityTier::PerBandGridNew
                    }
                );
                let reason =
                    pancetta_tui::dx_reason::reason_for(row, None, chrono::Utc::now()).summary();
                assert_eq!(reason.contains("grid FN42 may be new"), !worked_grid);
                assert_eq!(reason.contains("QSO failed this session"), worked_grid);
            }
        }
        // Never borrow another station's or another band's grid. A new
        // grid-bearing frame replaces the remembered value for its own key.
        for (call, band) in [
            (Some("K2ABC"), 14.074),
            (Some("K1ABC"), 7.074),
            (None, 14.074),
        ] {
            assert_eq!(super::display_grid(&mut last_grids, call, None, band), None);
        }
        assert_eq!(
            super::display_grid(&mut last_grids, Some("K1ABC"), Some("FN43"), 14.074).as_deref(),
            Some("FN43")
        );
        assert_eq!(
            super::display_grid(&mut last_grids, Some("K1ABC"), None, 14.074).as_deref(),
            Some("FN43")
        );
    }

    #[test]
    fn relay_cache_expires_and_caps_entries() {
        use std::time::{Duration, Instant};

        let start = Instant::now();
        let mut cache = super::RelayCache::new(Duration::from_secs(60), 3);
        cache.insert_at("K1ABC".to_owned(), 1, start);
        assert_eq!(cache.get_at(&"K1ABC".to_owned(), start), Some(&1));
        // Past the TTL an entry reads as gone even before it is pruned.
        let late = start + Duration::from_secs(61);
        assert_eq!(cache.get_at(&"K1ABC".to_owned(), late), None);

        // Going over the cap prunes expired entries first...
        for (i, call) in ["K2ABC", "K3ABC", "K4ABC"].into_iter().enumerate() {
            cache.insert_at(call.to_owned(), i, late);
        }
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.get_at(&"K1ABC".to_owned(), start), None);

        // ...then the oldest live ones.
        let later = late + Duration::from_secs(1);
        cache.insert_at("K5ABC".to_owned(), 5, later);
        cache.insert_at("K6ABC".to_owned(), 6, later);
        assert_eq!(cache.len(), 3);
        assert_eq!(cache.get_at(&"K5ABC".to_owned(), later), Some(&5));
        assert_eq!(cache.get_at(&"K6ABC".to_owned(), later), Some(&6));
    }

    #[tokio::test]
    async fn qso_outcome_rescores_the_row_from_fresh_failure_history() {
        use pancetta_tui::{app::App, tui_runner::TuiMessage, Config, DecodedMessageView};

        let lookup = crate::priority_evaluator::CachedStationLookup::new();
        let scorer = pancetta_qso::PriorityScorer::new(
            pancetta_qso::priority::PriorityWeights::default(),
            Box::new(lookup.clone()),
        );
        let mut app = App::new(Config::default(), None).await.unwrap();
        let (priority_score, priority_factors) =
            super::display_rank(&scorer, Some("K1ABC/P"), Some("FN42"), -3.0, 14.074);
        app.add_decoded_message(DecodedMessageView {
            timestamp: chrono::Utc::now(),
            frequency: 14.074,
            mode: "FT8".into(),
            snr: -3,
            delta_time: 0.0,
            delta_freq: 1500.0,
            call_sign: Some("K1ABC/P".into()),
            grid_square: Some("FN42".into()),
            message: "CQ K1ABC/P FN42".into(),
            distance: None,
            bearing: None,
            slot_parity: None,
            is_directed_at_us: false,
            worked_before: false,
            needed: false,
            atno: false,
            band_needed: false,
            priority_score,
            priority_factors,
            is_own_tx: false,
        })
        .await
        .unwrap();
        let before = app.dx_stations["K1ABC/P"].priority_score;
        let heard_at = app.dx_stations["K1ABC/P"].local_rank.unwrap().at;

        let mut last_inputs = super::RelayCache::default();
        last_inputs.insert(
            "K1ABC/P".to_owned(),
            super::RankInputs {
                grid: Some("FN42".into()),
                snr_db: -3.0,
                dial_mhz: 14.074,
            },
        );
        // A QSO with a station never decoded here has no row to rescore.
        assert!(super::outcome_rank_refresh(&scorer, &last_inputs, "K2ABC").is_empty());

        // The QSO task records the failure, then emits the outcome.
        lookup.record_failure("K1ABC/P");
        // The outcome may name the station in hash-rendered form.
        let refreshes = super::outcome_rank_refresh(&scorer, &last_inputs, "<K1ABC/P>");
        assert_eq!(refreshes.len(), 1);
        let Some(TuiMessage::DxRankRefresh {
            callsign,
            score,
            factors,
        }) = refreshes.into_iter().next()
        else {
            panic!("a decoded station's QSO outcome must rescore its row");
        };
        app.refresh_local_rank(&callsign, score, factors);

        let row = &app.dx_stations["K1ABC/P"];
        assert!(
            row.priority_score < before,
            "{} >= {before}",
            row.priority_score
        );
        let rank = row.local_rank.unwrap();
        assert!(rank.factors.unwrap().recent_failure);
        assert_eq!(rank.at, heard_at, "a rescore is not a new decode");
        let reason = pancetta_tui::dx_reason::reason_for(row, None, chrono::Utc::now()).summary();
        assert!(reason.contains("QSO failed this session"), "{reason}");
    }

    #[test]
    fn display_rank_carries_factors_from_the_shared_lookup() {
        let lookup = crate::priority_evaluator::CachedStationLookup::new();
        let scorer = pancetta_qso::PriorityScorer::new(
            pancetta_qso::priority::PriorityWeights::default(),
            Box::new(lookup.clone()),
        );
        let before = super::display_rank(&scorer, Some("K1ABC/P"), None, -3.0, 14.074);
        lookup.record_failure("K1ABC/P");
        let after = super::display_rank(&scorer, Some("K1ABC/P"), None, -3.0, 14.074);
        let factors = after.1.unwrap();
        assert!(factors.recent_failure);
        assert_eq!(
            factors.portable_suffix,
            Some(pancetta_qso::priority::PortableSuffix::P)
        );
        assert!(after.0.unwrap() < before.0.unwrap());
    }

    #[test]
    fn display_rank_is_none_without_a_callsign() {
        let scorer = pancetta_qso::PriorityScorer::new(
            pancetta_qso::priority::PriorityWeights::default(),
            Box::new(pancetta_qso::priority::NullLookup),
        );
        assert_eq!(
            super::display_rank(&scorer, None, None, -7.0, 14.074),
            (None, None)
        );
    }

    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicU8};

    /// Shared fixture for the `resolve_nudge_tx_offset` cases: an empty
    /// mailbox and an unset CQ flag.
    #[allow(clippy::type_complexity)]
    fn nudge_fixture() -> (
        std::sync::Mutex<Vec<pancetta_qso::qso_manager::OffsetActionRequest>>,
        AtomicBool,
    ) {
        (std::sync::Mutex::new(Vec::new()), AtomicBool::new(false))
    }

    fn tx_freq_mode_atomic(mode: pancetta_core::TxFreqMode) -> AtomicU8 {
        AtomicU8::new(mode.as_u8())
    }

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
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&std::collections::HashSet::from([qso_id])),
            None,
            None,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::ActiveQso(qso_id),
            "must resolve to the active QSO's id"
        );
        let pending = pending_qso_offset_requests.lock().unwrap();
        assert_eq!(pending.len(), 1, "exactly one offset request queued");
        assert_eq!(pending[0].qso_id, qso_id);
        assert!(
            matches!(
                pending[0].action,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz } if avoid_hz == 920.0
            ),
            "must be a Switch{{avoid_hz}} keyed off the QSO's CURRENT offset, got {:?}",
            pending[0].action
        );
        assert_eq!(
            pending[0].origin.raised_at_generation(),
            None,
            "an operator-forced `u` nudge carries no staleness token — it is \
             current by construction and must never be discarded as stale \
             (PAN-72 finding 8)"
        );
        assert!(
            !pending_cq_offset_nudge.load(Ordering::Relaxed),
            "the CQ-hunting fallback flag must NOT be set when an active QSO was nudged"
        );
    }

    /// PAN-72 final review (finding 2): in Hold mode the active-QSO nudge
    /// must be a declared no-op. Queueing a `Switch` would route through
    /// `allocate_smart_frequency`, whose Hold early-return ignores `avoid_hz`
    /// and yields the parked offset -- silently doing nothing, or worse,
    /// yanking an intentionally-held QSO onto the park offset, while the
    /// status line claimed a real nudge. The CQ fallback must NOT be armed
    /// as a consolation prize either: an active QSO exists, so CQ hunting is
    /// simply the wrong target.
    #[test]
    fn nudge_is_a_declared_no_op_for_an_active_qso_in_hold_mode() {
        let qso_id = pancetta_qso::QsoId::new_v4();
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([key.clone()]);
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key.clone(), 920.0)]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Hold),
            &active,
            Some(&std::collections::HashSet::from([qso_id])),
            None,
            None,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::HeldNoOp,
            "Hold mode must report a no-op, not a phantom nudge"
        );
        assert!(
            pending_qso_offset_requests.lock().unwrap().is_empty(),
            "Hold mode must not queue an offset request -- the allocator's \
             Hold branch ignores avoid_hz and returns the parked offset"
        );
        assert!(
            !pending_cq_offset_nudge.load(Ordering::Relaxed),
            "Hold mode must not arm the CQ fallback either -- there IS an \
             active QSO, so CQ hunting is the wrong target"
        );
    }

    /// PAN-72 (Codex round 2 on PR #350, finding 4): `active_tx_qsos` retains
    /// a COMPLETED QSO for 45 seconds so its trailing 73 can transmit. Nudging
    /// that entry told the operator "Nudging active QSO" while the drain later
    /// refused it with `QsoNotActive` and nothing moved. With the QSO engine
    /// reporting no live QSOs, `u` must fall through to the CQ fallback.
    #[test]
    fn nudge_ignores_a_completed_qso_still_inside_its_trailing_73_grace() {
        let qso_id = pancetta_qso::QsoId::new_v4();
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([key.clone()]);
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key.clone(), 920.0)]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();
        // The QSO engine says nothing is active: the snapshot entry is
        // grace-window-only.
        let live: std::collections::HashSet<pancetta_qso::QsoId> = std::collections::HashSet::new();

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&live),
            None,
            None,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::CqNudgeArmed,
            "a terminal grace-window entry must not be reported as an active QSO"
        );
        assert!(
            pending_qso_offset_requests.lock().unwrap().is_empty(),
            "no offset request may be queued for a QSO the drain will refuse"
        );
    }

    /// The concurrent-QSO half of the same finding: a completed entry must not
    /// SHADOW a genuinely live one just because it happens to come first out
    /// of the snapshot set.
    #[test]
    fn nudge_picks_the_live_qso_over_a_completed_one() {
        let dead = pancetta_qso::QsoId::new_v4();
        let alive = pancetta_qso::QsoId::new_v4();
        let dead_key = crate::coordinator::active_tx_qso_key(&dead.to_string());
        let alive_key = crate::coordinator::active_tx_qso_key(&alive.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([dead_key.clone(), alive_key.clone()]);
        let active_tx_offsets = std::sync::RwLock::new(std::collections::HashMap::from([
            (dead_key, 920.0),
            (alive_key, 1650.0),
        ]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();
        let live: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([alive]);

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&live),
            None,
            None,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::ActiveQso(alive),
            "the live QSO must be selected regardless of set iteration order"
        );
        let pending = pending_qso_offset_requests.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].qso_id, alive);
        assert!(
            matches!(
                pending[0].action,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz } if avoid_hz == 1650.0
            ),
            "avoid_hz must be the LIVE QSO's own offset, got {:?}",
            pending[0].action
        );
        assert!(!pending_cq_offset_nudge.load(Ordering::Relaxed));
    }

    /// PAN-72 (Codex round 1 on PR #350, finding 4): with NO active QSO and
    /// the mode on Hold, `u` must report a declared no-op too -- not a
    /// phantom CQ nudge.
    ///
    /// `decide_at` computes `should_switch = self.tx_freq_auto() && (...)`,
    /// so a flag armed in Hold is consumed and discarded with nothing to show
    /// for it. Reporting `CqNudgeArmed` there told the operator a nudge was
    /// under way when it provably could not be.
    #[test]
    fn nudge_is_a_declared_no_op_with_no_active_qso_in_hold_mode() {
        let active: std::collections::HashSet<String> = std::collections::HashSet::new();
        let active_tx_offsets = std::sync::RwLock::new(std::collections::HashMap::new());
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Hold),
            &active,
            None,
            None,
            None,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::HeldNoOp,
            "Hold with no active QSO must report a no-op, not a CQ nudge the \
             downstream tx_freq_auto() gate will silently discard"
        );
        assert!(
            !pending_cq_offset_nudge.load(Ordering::Relaxed),
            "Hold must not arm the CQ fallback flag at all"
        );
        assert!(pending_qso_offset_requests.lock().unwrap().is_empty());
    }

    /// PAN-72: `NudgeTxOffset` in Auto with no active QSO ARMS the one-shot
    /// `pending_cq_offset_nudge` flag (the CQ-hunting fallback) and must NOT
    /// touch `pending_qso_offset_requests`. Whether it produces a real move
    /// still depends on the autonomous operator entering its CQ branch on a
    /// later cycle -- nothing here can see or promise that, which is why the
    /// outcome is named `CqNudgeArmed` (see `resolve_nudge_tx_offset`'s doc
    /// comment).
    #[test]
    fn nudge_sets_cq_flag_when_no_active_qso() {
        let active: std::collections::HashSet<String> = std::collections::HashSet::new();
        let active_tx_offsets = std::sync::RwLock::new(std::collections::HashMap::new());
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            None,
            None,
            None,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::CqNudgeArmed,
            "no active QSO to resolve to"
        );
        assert!(
            pending_cq_offset_nudge.load(Ordering::Relaxed),
            "the CQ-hunting fallback flag must be set when no QSO is active"
        );
        assert!(
            pending_qso_offset_requests.lock().unwrap().is_empty(),
            "must not queue an offset request when there's no active QSO"
        );
    }

    /// PAN-72 Fix A (Codex round 10, thread on `autonomous.rs:1829`),
    /// superseding round 8's redesign: the target is a live, active QSO AND
    /// an autonomous self-CQ (`CallInitiation::Auto`, `QsoState::CallingCq`).
    /// Round 8 routed this through `pending_cq_offset_nudge` instead of
    /// `pending_qso_offset_requests`, reasoning that
    /// `rearm_manual_calls_at` only retransmits `CallingCq` for
    /// `CallInitiation::Manual` — but round 10 found `pending_cq_offset_
    /// nudge` structurally can never fire for this exact case (`decide_at`'s
    /// CQ-hunting branch is gated on `active_qso_count == 0`, and the
    /// `CallingCq` being nudged IS that one active QSO). Fix A routes an
    /// auto-CQ target through the SAME `pending_qso_offset_requests` path as
    /// any other target instead, relying on `QsoManager::
    /// apply_tx_offset_switch`'s new one-shot retransmission (independent of
    /// `rearm_manual_calls_at`'s own Manual-only cadence, which this fix does
    /// NOT touch) to actually resend the CQ at the new offset.
    ///
    /// `rearm_eligible_ids: Some(empty)` isolates this: the only way this
    /// target can match is through `auto_calling_cq_ids`, proving Fix A's
    /// routing specifically, not the unrelated `rearm_eligible_ids: None`
    /// pass-through convention another test covers.
    #[test]
    fn nudge_forces_switch_for_an_autonomous_self_cq_too() {
        let qso_id = pancetta_qso::QsoId::new_v4();
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([key.clone()]);
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key, 920.0)]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();
        let live: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([qso_id]);
        let auto_calling_cq: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([qso_id]);
        let rearm_eligible: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::new();

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&live),
            Some(&auto_calling_cq),
            Some(&rearm_eligible),
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::ActiveQso(qso_id),
            "an autonomous self-CQ must now be nudged like an ordinary \
             active QSO -- the CQ-nudge fallback could never actually fire \
             for this case"
        );
        let pending = pending_qso_offset_requests.lock().unwrap();
        assert_eq!(pending.len(), 1, "exactly one offset request queued");
        assert_eq!(pending[0].qso_id, qso_id);
        assert!(
            matches!(
                pending[0].action,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz } if avoid_hz == 920.0
            ),
            "must be a Switch{{avoid_hz}} keyed off the QSO's CURRENT offset, got {:?}",
            pending[0].action
        );
        assert!(
            !pending_cq_offset_nudge.load(Ordering::Relaxed),
            "the CQ-hunting fallback flag must NOT be set -- this QSO IS \
             the active QSO being nudged, not a CQ-hunting cycle"
        );
    }

    /// The control case for the fix above: a MANUAL-initiated `CallingCq` is
    /// present in `live_qso_ids` but NOT `auto_calling_cq_ids` (only
    /// `CallInitiation::Auto` QSOs go in that set) -- it DOES retransmit via
    /// `rearm_manual_calls_at`, so it must keep today's `ActiveQso` behavior
    /// unchanged.
    #[test]
    fn nudge_still_forces_switch_for_a_manual_calling_cq() {
        let qso_id = pancetta_qso::QsoId::new_v4();
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([key.clone()]);
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key, 920.0)]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();
        let live: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([qso_id]);
        // Present in live_qso_ids but this QSO is Manual-initiated, so it is
        // NOT in auto_calling_cq_ids.
        let auto_calling_cq: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::new();
        // A Manual-initiated CallingCq IS one of `rearm_manual_calls_at`'s
        // eligible states.
        let rearm_eligible: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([qso_id]);

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&live),
            Some(&auto_calling_cq),
            Some(&rearm_eligible),
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::ActiveQso(qso_id),
            "a Manual-initiated CallingCq must keep the existing ActiveQso \
             behavior -- it DOES retransmit, unlike an autonomous self-CQ"
        );
        let pending = pending_qso_offset_requests.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].qso_id, qso_id);
        assert!(!pending_cq_offset_nudge.load(Ordering::Relaxed));
    }

    /// PAN-72 round-9 fix (Codex round 9, finding 2): the only live QSO is in
    /// `WaitingForConfirmation` -- present in `live_qso_ids` (it IS active)
    /// but NOT in `rearm_eligible_ids`, because
    /// `QsoManager::rearm_manual_calls_at`'s `match &progress.state` block
    /// falls to `_ => continue` for this state -- no frame would ever be
    /// re-sent. The old code selected it as an ordinary `ActiveQso` target
    /// anyway, silently updating offset metadata while reporting success
    /// with nothing ever transmitted at the new offset. Must report
    /// `NoEligibleTarget` and touch NEITHER mailbox -- not
    /// `pending_qso_offset_requests` (there is nothing to commit), and not
    /// the CQ-nudge fallback either (there IS a live QSO; CQ-hunting is
    /// simply the wrong target for it, same reasoning as the Hold-mode
    /// no-op).
    #[test]
    fn nudge_reports_no_eligible_target_for_a_non_rearmable_state() {
        let qso_id = pancetta_qso::QsoId::new_v4();
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([key.clone()]);
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key, 920.0)]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();
        let live: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([qso_id]);
        // Live, but WaitingForConfirmation is not one of
        // `rearm_manual_calls_at`'s eligible states -- the set built at the
        // call site would never include it.
        let rearm_eligible: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::new();

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&live),
            None,
            Some(&rearm_eligible),
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::NoEligibleTarget,
            "a live QSO whose current state cannot retransmit must not be \
             reported as a successful nudge"
        );
        assert!(
            pending_qso_offset_requests.lock().unwrap().is_empty(),
            "nothing to commit -- rearm_manual_calls_at would never re-send \
             a frame for this state"
        );
        assert!(
            !pending_cq_offset_nudge.load(Ordering::Relaxed),
            "must not consume the CQ-hunting fallback either -- there IS a \
             live QSO, so CQ hunting is the wrong target for it"
        );
    }

    /// Regression companion: `RespondingToCq` and `SendingReport` ARE in
    /// `rearm_manual_calls_at`'s eligible-state list for any initiation, so
    /// they must keep resolving to `ActiveQso` exactly as before -- the
    /// round-9 restriction must not become an over-broad `None`-by-default
    /// that blocks every state.
    #[test]
    fn nudge_still_resolves_active_qso_for_rearm_eligible_states() {
        let qso_id = pancetta_qso::QsoId::new_v4();
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([key.clone()]);
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key, 1650.0)]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();
        let live: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([qso_id]);
        let rearm_eligible: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([qso_id]);

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&live),
            None,
            Some(&rearm_eligible),
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::ActiveQso(qso_id),
            "RespondingToCq/SendingReport (or any other rearm-eligible \
             state) must still nudge normally"
        );
        let pending = pending_qso_offset_requests.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].qso_id, qso_id);
        assert!(!pending_cq_offset_nudge.load(Ordering::Relaxed));
    }

    /// `rearm_eligible_ids: None` (the Qso component wasn't up to build the
    /// set) must preserve pre-round-9 behavior -- same convention as
    /// `live_qso_ids: None` above -- not retroactively block every nudge.
    #[test]
    fn nudge_with_no_rearm_eligibility_info_preserves_prior_behavior() {
        let qso_id = pancetta_qso::QsoId::new_v4();
        let key = crate::coordinator::active_tx_qso_key(&qso_id.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([key.clone()]);
        let active_tx_offsets =
            std::sync::RwLock::new(std::collections::HashMap::from([(key, 920.0)]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&std::collections::HashSet::from([qso_id])),
            None,
            None,
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(result, NudgeOutcome::ActiveQso(qso_id));
    }

    /// PAN-72 Fix D (Codex round 10, thread on `tui_relay.rs:3134`): round
    /// 9's Fix C added the eligibility check, but target SELECTION still did
    /// `.find()` on liveness alone, checking eligibility only on whatever
    /// arbitrary (`HashSet` iteration order) first live match came back. With
    /// two concurrent live QSOs -- one ineligible
    /// (`WaitingForConfirmation`-shaped: live, not rearm-eligible, not an
    /// auto-CQ), one eligible (`SendingReport`-shaped) -- the function could
    /// return `NoEligibleTarget` even though a perfectly good target exists,
    /// purely based on which key the `HashSet` visited first.
    ///
    /// The fix folds eligibility into the SAME predicate the search uses, so
    /// it naturally continues past an ineligible live QSO to one that
    /// qualifies. This is deterministic BY CONSTRUCTION regardless of which
    /// key `HashSet` iteration visits first -- exactly one of the two ids
    /// satisfies the combined predicate, so `.find()` returns it no matter
    /// which order the set is walked in.
    #[test]
    fn nudge_finds_the_eligible_qso_even_when_an_ineligible_one_shares_the_snapshot() {
        let ineligible = pancetta_qso::QsoId::new_v4();
        let eligible = pancetta_qso::QsoId::new_v4();
        let ineligible_key = crate::coordinator::active_tx_qso_key(&ineligible.to_string());
        let eligible_key = crate::coordinator::active_tx_qso_key(&eligible.to_string());
        let active: std::collections::HashSet<String> =
            std::collections::HashSet::from([ineligible_key.clone(), eligible_key.clone()]);
        let active_tx_offsets = std::sync::RwLock::new(std::collections::HashMap::from([
            (ineligible_key, 920.0),
            (eligible_key, 1650.0),
        ]));
        let (pending_qso_offset_requests, pending_cq_offset_nudge) = nudge_fixture();
        let live: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([ineligible, eligible]);
        // Neither is an autonomous self-CQ.
        let auto_calling_cq: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::new();
        // Only `eligible` is one of `rearm_manual_calls_at`'s eligible states
        // (e.g. `SendingReport`); `ineligible` (e.g. `WaitingForConfirmation`)
        // is live but not rearm-eligible.
        let rearm_eligible: std::collections::HashSet<pancetta_qso::QsoId> =
            std::collections::HashSet::from([eligible]);

        let result = resolve_nudge_tx_offset(
            &tx_freq_mode_atomic(pancetta_core::TxFreqMode::Auto),
            &active,
            Some(&live),
            Some(&auto_calling_cq),
            Some(&rearm_eligible),
            &active_tx_offsets,
            &pending_qso_offset_requests,
            &pending_cq_offset_nudge,
        );

        assert_eq!(
            result,
            NudgeOutcome::ActiveQso(eligible),
            "must select the eligible QSO regardless of HashSet iteration \
             order, never report NoEligibleTarget just because the \
             ineligible one might be visited first"
        );
        let pending = pending_qso_offset_requests.lock().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].qso_id, eligible);
        assert!(
            matches!(
                pending[0].action,
                pancetta_qso::qso_manager::OffsetAction::Switch { avoid_hz } if avoid_hz == 1650.0
            ),
            "avoid_hz must be the ELIGIBLE QSO's own offset, got {:?}",
            pending[0].action
        );
        assert!(!pending_cq_offset_nudge.load(Ordering::Relaxed));
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

    /// PAN-115: a rig CAT link that is up.
    fn healthy_rig() -> Arc<std::sync::atomic::AtomicU8> {
        Arc::new(std::sync::atomic::AtomicU8::new(
            crate::coordinator::hamlib::RigConnState::Connected.as_u8(),
        ))
    }

    /// PAN-115: an audio output that is calling back.
    fn audio_alive() -> Arc<std::sync::atomic::AtomicBool> {
        Arc::new(std::sync::atomic::AtomicBool::new(true))
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

    #[cfg(unix)]
    #[tokio::test]
    async fn bookmark_save_after_a_config_symlink_repoint_lands_in_the_new_target() {
        let dir = tempfile::tempdir().unwrap();
        let (link_dir, v1_dir, v2_dir) = (
            dir.path().join("link"),
            dir.path().join("v1"),
            dir.path().join("v2"),
        );
        for path in [&link_dir, &v1_dir, &v2_dir] {
            std::fs::create_dir(path).unwrap();
        }
        let v1 = v1_dir.join("pancetta.toml");
        let v2 = v2_dir.join("pancetta.toml");
        std::fs::write(&v1, "# v1\n").unwrap();
        std::fs::write(&v2, "# v2\n").unwrap();
        let link = link_dir.join("pancetta.toml");
        std::os::unix::fs::symlink(&v1, &link).unwrap();

        let config = Arc::new(tokio::sync::RwLock::new(pancetta_config::Config::default()));
        let (write_tx, _write_handle) = spawn_config_file_write_worker();
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let (bookmark_tx, _bookmark_handle) =
            spawn_bookmark_mutation_worker(config, write_tx, tui_msg_tx, link.clone(), true);

        let save = |name: &str| {
            bookmark_tx
                .send(BookmarkMutation::Save(pancetta_config::rig::RigBookmark {
                    name: name.to_string(),
                    model: "FTdx10".to_string(),
                    port: "/dev/ttyUSB0".to_string(),
                    baud_rate: 38400,
                    ptt_method: pancetta_config::rig::PttMethod::None,
                }))
                .unwrap();
        };
        async fn next_status(
            rx: &crossbeam_channel::Receiver<pancetta_tui::tui_runner::TuiMessage>,
        ) -> String {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if let Ok(pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                        status, ..
                    }) = rx.try_recv()
                    {
                        return status;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .expect("bookmark worker must report a status")
        }

        save("Before");
        assert_eq!(next_status(&tui_msg_rx).await, "Saved bookmark 'Before'");
        assert!(std::fs::read_to_string(&v1).unwrap().contains("Before"));

        let tmp_link = link_dir.join(".pancetta.toml.tmp");
        std::os::unix::fs::symlink(&v2, &tmp_link).unwrap();
        std::fs::rename(&tmp_link, &link).unwrap();

        save("After");
        assert_eq!(next_status(&tui_msg_rx).await, "Saved bookmark 'After'");
        let new_contents = std::fs::read_to_string(&v2).unwrap();
        let parsed: toml::Value = toml::from_str(&new_contents).unwrap();
        let bookmarks = parsed["rig"]["bookmarks"].as_array().unwrap();
        assert!(bookmarks
            .iter()
            .any(|item| item["name"].as_str() == Some("After")));
        assert!(new_contents.contains("# v2"));
        assert!(!std::fs::read_to_string(&v1).unwrap().contains("After"));
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), v2);
    }

    /// PAN-200 review (Codex P2): the running bookmark list came from v1.
    /// After the link is repointed at v2, a save applies to v2's own list
    /// instead of replacing it with the running one.
    #[cfg(unix)]
    #[tokio::test]
    async fn bookmark_save_after_a_config_symlink_repoint_keeps_the_new_targets_bookmarks() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join("v1.toml");
        let v2 = dir.path().join("v2.toml");
        let bookmark = |name: &str| pancetta_config::rig::RigBookmark {
            name: name.to_string(),
            model: "FTdx10".to_string(),
            port: "/dev/ttyUSB0".to_string(),
            baud_rate: 38400,
            ptt_method: pancetta_config::rig::PttMethod::None,
        };
        let names = |list: &[pancetta_config::rig::RigBookmark]| {
            list.iter().map(|b| b.name.clone()).collect::<Vec<_>>()
        };
        pancetta_config::Config::default()
            .set_rig_bookmarks_in_file(&v1, &[bookmark("V1Only")])
            .unwrap();
        pancetta_config::Config::default()
            .set_rig_bookmarks_in_file(&v2, &[bookmark("V2Only")])
            .unwrap();
        let link = dir.path().join("pancetta.toml");
        std::os::unix::fs::symlink(&v1, &link).unwrap();

        let mut config = pancetta_config::Config::default();
        config.rig.bookmarks = Some(vec![bookmark("V1Only")]);
        let config = Arc::new(tokio::sync::RwLock::new(config));
        let (write_tx, _write_handle) = spawn_config_file_write_worker();
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let (bookmark_tx, _bookmark_handle) = spawn_bookmark_mutation_worker(
            config.clone(),
            write_tx,
            tui_msg_tx,
            link.clone(),
            true,
        );

        let tmp_link = dir.path().join(".pancetta.toml.tmp");
        std::os::unix::fs::symlink(&v2, &tmp_link).unwrap();
        std::fs::rename(&tmp_link, &link).unwrap();

        bookmark_tx
            .send(BookmarkMutation::Save(bookmark("After")))
            .unwrap();
        let status = tokio::time::timeout(std::time::Duration::from_secs(5), async {
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
        .expect("bookmark worker must report a status");

        assert_eq!(status, "Saved bookmark 'After'");
        assert_eq!(
            names(&read_rig_bookmarks_in_file(&v2).unwrap().unwrap()),
            ["V2Only", "After"]
        );
        assert_eq!(
            names(&read_rig_bookmarks_in_file(&v1).unwrap().unwrap()),
            ["V1Only"]
        );
        assert_eq!(
            names(config.read().await.rig.effective_bookmarks()),
            ["V2Only", "After"]
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_config_write_target_follows_a_repointed_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join("v1.toml");
        let v2 = dir.path().join("v2.toml");
        std::fs::write(&v1, "# v1\n").unwrap();
        std::fs::write(&v2, "# v2\n").unwrap();
        let link = dir.path().join("pancetta.toml");
        std::os::unix::fs::symlink(&v1, &link).unwrap();
        assert_eq!(
            resolve_config_write_target(&link).unwrap(),
            v1.canonicalize().unwrap()
        );
        let tmp_link = dir.path().join(".pancetta.toml.tmp");
        std::os::unix::fs::symlink(&v2, &tmp_link).unwrap();
        std::fs::rename(&tmp_link, &link).unwrap();
        assert_eq!(
            resolve_config_write_target(&link).unwrap(),
            v2.canonicalize().unwrap()
        );
    }

    #[cfg(unix)]
    #[test]
    fn resolve_config_write_target_refuses_a_dangling_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("pancetta.toml");
        std::os::unix::fs::symlink(dir.path().join("missing.toml"), &link).unwrap();
        assert!(resolve_config_write_target(&link)
            .unwrap_err()
            .to_string()
            .contains("is a symlink to a missing file"));
        assert!(std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink());
    }

    #[test]
    fn resolve_config_write_target_returns_a_missing_path_as_given() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub/pancetta.toml");
        assert_eq!(resolve_config_write_target(&path).unwrap(), path);
    }

    /// Poll `rx` (never a blocking `recv()` -- see the bookmark-worker test
    /// above) for the next `RigConfigUpdate`, failing after 5s.
    async fn next_rig_config_update(
        rx: &crossbeam_channel::Receiver<pancetta_tui::tui_runner::TuiMessage>,
    ) -> (
        Vec<String>,
        String,
        String,
        Vec<pancetta_config::rig::RigBookmark>,
    ) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(pancetta_tui::tui_runner::TuiMessage::RigConfigUpdate {
                    available_ports,
                    current_model,
                    current_port,
                    bookmarks,
                    ..
                }) = rx.try_recv()
                {
                    return (available_ports, current_model, current_port, bookmarks);
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a rig config refresh must reply within 5s")
    }

    fn rig_refresh_config(
        model: &str,
        port: &str,
    ) -> Arc<tokio::sync::RwLock<pancetta_config::Config>> {
        let mut cfg = pancetta_config::Config::default();
        cfg.rig.model = model.to_string();
        cfg.rig.interface.port = port.to_string();
        Arc::new(tokio::sync::RwLock::new(cfg))
    }

    fn ports(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| s.to_string()).collect()
    }

    /// A `SelectRig` count no command has bumped yet.
    fn no_selects() -> Arc<AtomicU64> {
        Arc::new(AtomicU64::new(0))
    }

    /// PAN-60: the ticket's scenario at the coordinator boundary -- a port
    /// that appears between two refreshes (a rig plugged in after startup)
    /// is in the second reply, alongside the configured model and port.
    #[tokio::test]
    async fn rig_config_refresh_lists_a_port_plugged_in_after_the_previous_refresh() {
        let config = rig_refresh_config("FTdx10", "/dev/ttyUSB0");
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let in_flight = Arc::new(AtomicBool::new(false));

        let first = spawn_rig_config_refresh(
            config.clone(),
            tui_msg_tx.clone(),
            in_flight.clone(),
            no_selects(),
            || ports(&["/dev/ttyUSB0"]),
        )
        .expect("nothing is in flight, so the first refresh must spawn");
        first.await.unwrap();
        let (first_ports, _, _, _) = next_rig_config_update(&tui_msg_rx).await;
        assert_eq!(first_ports, ports(&["/dev/ttyUSB0"]));

        let second = spawn_rig_config_refresh(
            config.clone(),
            tui_msg_tx.clone(),
            in_flight.clone(),
            no_selects(),
            || ports(&["/dev/ttyUSB0", "/dev/ttyUSB1"]),
        )
        .expect("the first refresh finished, so the second must spawn");
        second.await.unwrap();
        let (available_ports, current_model, current_port, _) =
            next_rig_config_update(&tui_msg_rx).await;
        assert_eq!(
            available_ports,
            ports(&["/dev/ttyUSB0", "/dev/ttyUSB1"]),
            "a port plugged in after the previous refresh must be listed"
        );
        assert_eq!(current_model, "FTdx10");
        assert_eq!(current_port, "/dev/ttyUSB0");
    }

    /// PAN-60: the relay loop that calls the helper also carries
    /// OperatorEmergencyStop/StopTx/TogglePtt/AbortQso, so the helper must
    /// return while the enumeration is still running.
    #[tokio::test]
    async fn rig_config_refresh_returns_before_the_enumeration_finishes() {
        let config = rig_refresh_config("FTdx10", "/dev/ttyUSB0");
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let in_flight = Arc::new(AtomicBool::new(false));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();

        let handle =
            spawn_rig_config_refresh(config, tui_msg_tx, in_flight, no_selects(), move || {
                gate_rx.recv().unwrap();
                ports(&["/dev/ttyUSB0"])
            });
        assert!(
            handle.is_some(),
            "the helper must return its handle while the enumeration is still gated"
        );
        assert!(
            matches!(
                tui_msg_rx.try_recv(),
                Err(crossbeam_channel::TryRecvError::Empty)
            ),
            "no reply can exist before the enumeration finishes"
        );

        gate_tx.send(()).unwrap();
        let (available_ports, _, _, _) = next_rig_config_update(&tui_msg_rx).await;
        assert_eq!(available_ports, ports(&["/dev/ttyUSB0"]));
        handle.unwrap().await.unwrap();
    }

    /// PAN-60: a request made while a refresh is in flight is answered by
    /// that refresh's reply -- it spawns nothing and never enumerates, so a
    /// stalled serial driver parks at most one blocking-pool thread.
    #[tokio::test]
    async fn rig_config_refresh_coalesces_a_request_made_while_one_is_in_flight() {
        let config = rig_refresh_config("FTdx10", "/dev/ttyUSB0");
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let in_flight = Arc::new(AtomicBool::new(false));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();

        let first = spawn_rig_config_refresh(
            config.clone(),
            tui_msg_tx.clone(),
            in_flight.clone(),
            no_selects(),
            move || {
                gate_rx.recv().unwrap();
                ports(&["/dev/ttyUSB0"])
            },
        )
        .expect("nothing is in flight, so the first refresh must spawn");

        let second = spawn_rig_config_refresh(
            config.clone(),
            tui_msg_tx.clone(),
            in_flight.clone(),
            no_selects(),
            || -> Vec<String> { panic!("a coalesced request must never enumerate") },
        );
        assert!(
            second.is_none(),
            "a request made while a refresh is in flight must be coalesced into it"
        );

        gate_tx.send(()).unwrap();
        first.await.unwrap();
        let _ = next_rig_config_update(&tui_msg_rx).await;
        assert!(
            !in_flight.load(Ordering::Acquire),
            "the finished refresh must clear the single-flight flag"
        );

        let third = spawn_rig_config_refresh(config, tui_msg_tx, in_flight, no_selects(), || {
            ports(&["/dev/ttyUSB0"])
        });
        assert!(
            third.is_some(),
            "once the in-flight refresh finished, a new request must spawn"
        );
        third.unwrap().await.unwrap();
    }

    /// PAN-60 (folds in the PAN-59 "no re-push after a switch" finding):
    /// config is read AFTER enumerating, so a rig switch or bookmark change
    /// committed while the enumeration ran is in the reply -- a reopen
    /// after a switch is always fresh.
    #[tokio::test]
    async fn rig_config_refresh_reads_config_after_enumerating() {
        let config = rig_refresh_config("FTdx10", "/dev/ttyUSB0");
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let in_flight = Arc::new(AtomicBool::new(false));
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();

        let handle = spawn_rig_config_refresh(
            config.clone(),
            tui_msg_tx,
            in_flight,
            no_selects(),
            move || {
                gate_rx.recv().unwrap();
                ports(&["/dev/ttyUSB0", "/dev/ttyUSB1"])
            },
        )
        .expect("nothing is in flight, so the refresh must spawn");

        {
            let mut cfg = config.write().await;
            cfg.rig.model = "IC-7300".to_string();
            cfg.rig.interface.port = "/dev/ttyUSB1".to_string();
            cfg.rig.bookmarks = Some(vec![pancetta_config::rig::RigBookmark {
                name: "Portable".to_string(),
                model: "IC-7300".to_string(),
                port: "/dev/ttyUSB1".to_string(),
                baud_rate: 19200,
                ptt_method: pancetta_config::rig::PttMethod::Cat,
            }]);
        }
        gate_tx.send(()).unwrap();

        let (available_ports, current_model, current_port, bookmarks) =
            next_rig_config_update(&tui_msg_rx).await;
        handle.await.unwrap();
        assert_eq!(available_ports, ports(&["/dev/ttyUSB0", "/dev/ttyUSB1"]));
        assert_eq!(
            current_model, "IC-7300",
            "the reply must carry the config as it was after enumerating, not before"
        );
        assert_eq!(current_port, "/dev/ttyUSB1");
        assert_eq!(bookmarks.len(), 1);
        assert_eq!(bookmarks[0].name, "Portable");
    }

    /// PAN-60 review: the reply's `select_seq` is read with the rig values,
    /// so a `SelectRig` committed while the enumeration ran shows up in
    /// both, and the TUI never takes pre-switch values as post-switch ones.
    #[tokio::test]
    async fn rig_config_refresh_stamps_the_select_count_read_with_the_config() {
        let config = rig_refresh_config("FTdx10", "/dev/ttyUSB0");
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let in_flight = Arc::new(AtomicBool::new(false));
        let select_seq = no_selects();
        let (gate_tx, gate_rx) = std::sync::mpsc::channel::<()>();

        let handle = spawn_rig_config_refresh(
            config.clone(),
            tui_msg_tx,
            in_flight,
            select_seq.clone(),
            move || {
                gate_rx.recv().unwrap();
                ports(&["/dev/ttyUSB0"])
            },
        )
        .expect("nothing is in flight, so the refresh must spawn");

        {
            // What the relay's `SelectRig` arm does under its write lock.
            let mut cfg = config.write().await;
            cfg.rig.model = "IC-7300".to_string();
            select_seq.fetch_add(1, Ordering::AcqRel);
        }
        gate_tx.send(()).unwrap();

        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                if let Ok(pancetta_tui::tui_runner::TuiMessage::RigConfigUpdate {
                    current_model,
                    select_seq,
                    ..
                }) = tui_msg_rx.try_recv()
                {
                    return (current_model, select_seq);
                }
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("a rig config refresh must reply within 5s");
        handle.await.unwrap();
        assert_eq!(
            reply,
            ("IC-7300".to_string(), 1),
            "the reply must carry the SelectRig count that matches its rig values"
        );
    }

    /// PAN-60: an enumerator panic still produces a reply (empty port list,
    /// so the TUI falls back to the configured port) and still clears the
    /// single-flight flag -- otherwise every later `i` would be coalesced
    /// into a refresh that never answers.
    #[tokio::test]
    async fn rig_config_refresh_still_replies_and_clears_the_flag_when_enumeration_panics() {
        let config = rig_refresh_config("FTdx10", "/dev/ttyUSB0");
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::unbounded();
        let in_flight = Arc::new(AtomicBool::new(false));

        let handle = spawn_rig_config_refresh(
            config,
            tui_msg_tx,
            in_flight.clone(),
            no_selects(),
            || -> Vec<String> { panic!("simulated enumeration failure") },
        )
        .expect("nothing is in flight, so the refresh must spawn");

        let (available_ports, current_model, current_port, _) =
            next_rig_config_update(&tui_msg_rx).await;
        assert!(
            available_ports.is_empty(),
            "a failed enumeration must reply with an empty list, got {available_ports:?}"
        );
        assert_eq!(current_model, "FTdx10");
        assert_eq!(current_port, "/dev/ttyUSB0");
        assert!(
            handle.await.is_ok(),
            "an enumerator panic must not propagate out of the refresh task"
        );
        assert!(
            !in_flight.load(Ordering::Acquire),
            "an enumerator panic must still clear the single-flight flag"
        );
    }

    /// PAN-60: the reply is sent while holding the config read lock, so it
    /// must never block -- a full TUI channel drops the reply rather than
    /// parking a runtime thread under a lock `SelectRig` needs to write.
    #[tokio::test]
    async fn rig_config_refresh_drops_its_reply_instead_of_blocking_on_a_full_tui_channel() {
        let config = rig_refresh_config("FTdx10", "/dev/ttyUSB0");
        let (tui_msg_tx, tui_msg_rx) = crossbeam_channel::bounded(1);
        tui_msg_tx
            .send(pancetta_tui::tui_runner::TuiMessage::StatusUpdate {
                component: "test".to_string(),
                status: "channel full".to_string(),
            })
            .unwrap();
        let in_flight = Arc::new(AtomicBool::new(false));

        let handle =
            spawn_rig_config_refresh(config, tui_msg_tx, in_flight.clone(), no_selects(), || {
                ports(&["/dev/ttyUSB0"])
            })
            .expect("nothing is in flight, so the refresh must spawn");

        tokio::time::timeout(std::time::Duration::from_secs(5), handle)
            .await
            .expect("a full TUI channel must never block the refresh task")
            .unwrap();
        assert_eq!(tui_msg_rx.len(), 1, "the reply must be dropped, not queued");
        assert!(
            matches!(
                tui_msg_rx.try_recv(),
                Ok(pancetta_tui::tui_runner::TuiMessage::StatusUpdate { .. })
            ),
            "the message already in the channel must be untouched"
        );
        assert!(
            !in_flight.load(Ordering::Acquire),
            "a dropped reply must still clear the single-flight flag"
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
            &healthy_rig(),
            &audio_alive(),
            &{
                let gain = super::super::receive_overload::GainSafety::default();
                gain.confirmed_off(0);
                std::sync::Arc::new(gain)
            },
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
                &healthy_rig(),
                &audio_alive(),
                &{
                    let gain = super::super::receive_overload::GainSafety::default();
                    gain.confirmed_off(0);
                    std::sync::Arc::new(gain)
                },
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
                &healthy_rig(),
                &audio_alive(),
                &{
                    let gain = super::super::receive_overload::GainSafety::default();
                    gain.confirmed_off(0);
                    std::sync::Arc::new(gain)
                },
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
                &healthy_rig(),
                &audio_alive(),
                &{
                    let gain = super::super::receive_overload::GainSafety::default();
                    gain.confirmed_off(0);
                    std::sync::Arc::new(gain)
                },
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
                &healthy_rig(),
                &audio_alive(),
                &{
                    let gain = super::super::receive_overload::GainSafety::default();
                    gain.confirmed_off(0);
                    std::sync::Arc::new(gain)
                },
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
                &healthy_rig(),
                &audio_alive(),
                &{
                    let gain = super::super::receive_overload::GainSafety::default();
                    gain.confirmed_off(0);
                    std::sync::Arc::new(gain)
                },
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
                    &healthy_rig(),
                    &audio_alive(),
                    &{
                        let gain = super::super::receive_overload::GainSafety::default();
                        gain.confirmed_off(0);
                        std::sync::Arc::new(gain)
                    },
                )
                .is_some(),
                "a pending command for {vfo:?} alone must still refuse a manual PTT key-up"
            );
        }
    }

    /// PAN-115 Gherkin 2, manual-PTT path: a key-up is refused with the
    /// exact operator-facing status while the rig CAT link is lost.
    #[test]
    fn ptt_on_refusal_blocks_a_key_up_while_the_rig_is_disconnected() {
        let tx_policy = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8()));
        let rig_conn_state = Arc::new(AtomicU8::new(
            crate::coordinator::hamlib::RigConnState::PollingFailed.as_u8(),
        ));
        assert_eq!(
            ptt_on_refusal(
                &tx_policy,
                &Arc::new(AtomicU32::new(0)),
                &Arc::new(AtomicBool::new(true)),
                &no_pending_frequency(),
                &no_pending(),
                &not_in_flight(),
                &rig_conn_state,
                &audio_alive(),
                &{
                    let gain = super::super::receive_overload::GainSafety::default();
                    gain.confirmed_off(0);
                    std::sync::Arc::new(gain)
                },
            ),
            Some("Can't key PTT — rig is disconnected".to_string())
        );
    }

    /// PAN-115 Gherkin 1, manual-PTT path: a key-up is refused with the
    /// exact operator-facing status while the audio output is dead.
    #[test]
    fn ptt_on_refusal_blocks_a_key_up_while_audio_output_is_dead() {
        let tx_policy = Arc::new(AtomicU8::new(pancetta_core::TxPolicy::Full.as_u8()));
        assert_eq!(
            ptt_on_refusal(
                &tx_policy,
                &Arc::new(AtomicU32::new(0)),
                &Arc::new(AtomicBool::new(true)),
                &no_pending_frequency(),
                &no_pending(),
                &not_in_flight(),
                &healthy_rig(),
                &Arc::new(AtomicBool::new(false)),
                &{
                    let gain = super::super::receive_overload::GainSafety::default();
                    gain.confirmed_off(0);
                    std::sync::Arc::new(gain)
                },
            ),
            Some("Can't key PTT — audio output device is dead".to_string())
        );
    }

    /// PAN-118, manual-PTT path: a key-up is refused with the exact
    /// operator-facing status while the effective TX dial sits outside the
    /// configured amateur band plan (15.000 MHz is inside no allocation).
    #[test]
    fn pan206_polled_out_of_band_split_refuses_manual_ptt() {
        assert_eq!(
            ptt_band_plan_refusal(
                &Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)),
                &Arc::new(std::sync::atomic::AtomicU64::new(15_000_000)),
                &pancetta_config::BandPlanConfig::default(),
            )
            .as_deref(),
            Some("Can't key PTT — TX dial 15000000 Hz is outside the configured amateur band plan"),
        );
    }

    #[test]
    fn pan206_in_band_polled_split_allows_manual_ptt() {
        assert_eq!(
            ptt_band_plan_refusal(
                &Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)),
                &Arc::new(std::sync::atomic::AtomicU64::new(14_078_000)),
                &pancetta_config::BandPlanConfig::default(),
            ),
            None,
        );
    }

    #[test]
    fn ptt_band_plan_refusal_names_the_out_of_band_dial() {
        assert_eq!(
            ptt_band_plan_refusal(
                &Arc::new(std::sync::atomic::AtomicU64::new(15_000_000)),
                &Arc::new(std::sync::atomic::AtomicU64::new(0)),
                &pancetta_config::BandPlanConfig::default(),
            ),
            Some(
                "Can't key PTT — TX dial 15000000 Hz is outside the configured amateur band plan"
                    .to_string()
            )
        );
    }

    /// PAN-118, manual-PTT path: an in-band dial keys, and the unknown-0
    /// sentinel dial (mock/rig-disabled dev modes) skips the check.
    #[test]
    fn ptt_band_plan_refusal_passes_in_band_and_unknown_dial() {
        assert_eq!(
            ptt_band_plan_refusal(
                &Arc::new(std::sync::atomic::AtomicU64::new(14_074_000)),
                &Arc::new(std::sync::atomic::AtomicU64::new(0)),
                &pancetta_config::BandPlanConfig::default(),
            ),
            None
        );
        assert_eq!(
            ptt_band_plan_refusal(
                &Arc::new(std::sync::atomic::AtomicU64::new(0)),
                &Arc::new(std::sync::atomic::AtomicU64::new(0)),
                &pancetta_config::BandPlanConfig::default(),
            ),
            None
        );
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
            pending_freq_drift_hz: Some(937.5),
            pending_freq_drift_since: Some(started),
        };
        let banner = map_qso_snapshot_item(&item);
        assert_eq!(banner.qso_id, "11111111-1111-1111-1111-111111111111");
        assert_eq!(banner.pending_freq_drift_hz, Some(937.5));
        assert_eq!(banner.pending_freq_drift_since, Some(started));
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
            pending_freq_drift_hz: None,
            pending_freq_drift_since: None,
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
