//! FT8 decoder component startup.
//!
//! Receives 12 kHz windows from the DSP stage, runs them through the
//! `ft8_lib` reference decoder plus the native `pancetta-ft8` AP-enhanced
//! decoder, and merges the two result sets (ft8_lib first, native fills
//! in any AP-only decodes the reference missed). Emits decoded messages
//! to:
//!   - the TUI via a dedicated crossbeam channel,
//!   - the Autonomous operator via the message bus,
//!   - the QSO state machine via the message bus,
//!   - PSKReporter via the message bus.
//!
//! Also generates the spectrogram-style waterfall (one matrix per window)
//! and forwards it to the TUI and the autonomous operator's frequency
//! allocator.

use anyhow::Result;
use pancetta_ft8::Ft8Decoder;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;
use tracing::{debug, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

impl super::ApplicationCoordinator {
    /// Start FT8 decoder with point-to-point channels.
    pub(crate) async fn start_ft8_pipeline(
        &mut self,
        ft8_rx: crossbeam_channel::Receiver<Vec<f32>>,
        ft8_to_tui_tx: crossbeam_channel::Sender<pancetta_ft8::DecodedMessage>,
        waterfall_tx: crossbeam_channel::Sender<Vec<Vec<f32>>>,
        health_total_decodes: Arc<std::sync::atomic::AtomicU64>,
    ) -> Result<()> {
        let span = span!(Level::INFO, "start_ft8");
        let _enter = span.enter();

        info!("Starting FT8 component");

        // hb-216 S2: read the shared Ft8Config. The tier probe (background
        // task spawned by coordinator::tier::initialize) may rewrite this
        // with the Slow-tier preset after measurement; the hot loop
        // re-reads it each iteration and rebuilds the decoder if
        // (max_decode_passes, osd_depth) changed.
        let initial_ft8_config = self.ft8_config.read().await.clone();
        let mut decoder = Ft8Decoder::new(initial_ft8_config.clone())?;
        let ft8_config_shared = self.ft8_config.clone();

        // hb-216 S2: scoped fast-path activation flag. Seeded from env at
        // startup; rewritten by the tier probe (Moderate/Slow → true).
        let scoped_fast_path = self.scoped_fast_path.clone();

        let shutdown = self.shutdown_signal.clone();
        let last_decode_timestamp = self.last_decode_timestamp.clone();
        let message_bus = self.message_bus.clone();
        let self_waterfall_to_auto_tx = self.waterfall_to_auto_tx.clone();

        // Read station callsign for AP decoding before moving into the thread
        let station_callsign = {
            let config = self.config.read().await;
            config.station.callsign.clone()
        };

        // Shared AP state updated by the QSO component
        let active_qso_ap = self.active_qso_ap.clone();

        // hb-091 scoped fast-path: shared partner-freq state. When Some,
        // and `PANCETTA_SCOPED_FAST_PATH=1` is set, the FT8 thread runs
        // a scoped Costas search at the partner's freq_bin BEFORE the
        // standard ft8_lib + native decode. Scoped completes in
        // ~329ms p50 / ~866ms p99 on M4 reference hardware (vs full
        // p50=862ms / p99=2332ms), reliably finishing inside the slot
        // budget. Standard pipeline still runs after as the
        // authoritative result; the QSO state machine deduplicates.
        //
        // hb-229 — QSO partner band-collapse: the same shared state is
        // ALSO consumed by the main native decode below. When a QSO is
        // in flight (and `PANCETTA_QSO_FILTER_OFF` is not set), the
        // main decode is narrowed to ±60 Hz around the partner. Pure
        // operational CPU win; same recall in the target band.
        let active_qso_freq_hz = self.active_qso_freq_hz.clone();

        // hb-229: cache the operator override once at thread start so
        // the hot loop doesn't pay a syscall on every window. The
        // env var is documented as set-at-startup; live re-reads would
        // race the QSO state machine anyway.
        let qso_filter_override_off = super::qso_filter::filter_disabled_by_env();
        if qso_filter_override_off {
            info!(
                "hb-229: QSO partner band-collapse disabled by {}=1",
                super::qso_filter::QSO_FILTER_OFF_ENV
            );
        }

        // hb-062: shared FP filter (Option<Arc<...>>). When Some, applied
        // between decode-merge and broadcast loop. None = no filtering.
        let fp_filter = self.fp_filter.clone();

        // Shared cross-slot state substrate (hb-048 / hb-057 / hb-173).
        // Populated post-FP-filter so the three downstream tables never
        // ingest decodes the continuity filter judged false.
        let cross_time_state = self.cross_time_state.clone();

        // hb-237: cross-sequence A7 callsign cache. Populated post-FP-filter
        // so the cache only ever ingests trusted callsigns; the trust-gate
        // is an additional defense (the spec calls out FP-amplification
        // risk if seed callsigns are FPs). The cache is read at the start
        // of each subsequent slot to surface opposite-parity seeds. Inert
        // until `Ft8Config::cross_sequence_a7_enabled` flips true.
        let cross_sequence_cache = self.cross_sequence_cache.clone();
        let cross_sequence_fp_filter = self.fp_filter.clone();

        // Run FT8 decoder on a dedicated thread to avoid tokio starvation
        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            info!("FT8 decoder thread started");

            // hb-216 S2: track the config tuple the current decoder was
            // built with. When the shared config changes (tier probe
            // landing a Slow preset), rebuild before the next decode.
            let mut last_max_passes = initial_ft8_config.max_decode_passes;
            let mut last_osd_depth = initial_ft8_config.osd_depth;

            // Create persistent AP state for enhanced decoding
            let my_call_ap = pancetta_ft8::MyCallAp::new(&station_callsign);
            if my_call_ap.is_none() {
                warn!(
                    "AP decoding: could not encode station callsign '{}', AP1+ disabled",
                    station_callsign
                );
            } else {
                info!(
                    "AP decoding: station callsign '{}' encoded for AP injection",
                    station_callsign
                );
            }
            let mut recent_pool: Vec<pancetta_ft8::RecentCallAp> = Vec::new();

            while !shutdown.load(Ordering::Acquire) {
                match ft8_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(window) => {
                        // Capture receipt time immediately — before any decode
                        // work — so parity tagging is invariant under decode
                        // latency. (If we captured now() after decode, a slow
                        // slot on a loaded MiniPC could push us into slot N+1
                        // and produce the wrong parity, causing the autonomous
                        // operator to TX in the same slot as the DX.)
                        let window_received_utc = chrono::Utc::now();

                        // hb-216 S2: re-check the shared Ft8Config. If the
                        // tier probe landed a Slow preset since the last
                        // window, rebuild the decoder. `try_read` keeps the
                        // hot loop non-blocking; on contention, we skip the
                        // check this iteration and pick it up on the next.
                        // hb-237: cache the cross-sequence A7 enable flag
                        // alongside the config-rebuild check so we read the
                        // shared Ft8Config at most once per window.
                        let mut cross_seq_enabled = false;
                        if let Ok(cfg_guard) = ft8_config_shared.try_read() {
                            let cur_max = cfg_guard.max_decode_passes;
                            let cur_osd = cfg_guard.osd_depth;
                            cross_seq_enabled = cfg_guard.cross_sequence_a7_enabled;
                            if cur_max != last_max_passes || cur_osd != last_osd_depth {
                                let new_cfg = cfg_guard.clone();
                                drop(cfg_guard);
                                match Ft8Decoder::new(new_cfg) {
                                    Ok(d) => {
                                        info!(
                                            "FT8 decoder rebuilt for tier preset: max_decode_passes={}, osd_depth={:?}",
                                            cur_max, cur_osd
                                        );
                                        decoder = d;
                                        last_max_passes = cur_max;
                                        last_osd_depth = cur_osd;
                                    }
                                    Err(e) => warn!(
                                        "FT8 decoder rebuild failed (keeping previous): {}",
                                        e
                                    ),
                                }
                            }
                        }

                        // hb-237 cross-sequence A7 — pre-decode hint read.
                        // When enabled, look up the prior slot's opposite-
                        // parity callsigns from the cross-sequence cache.
                        // The seeds are reported only (log + count) until
                        // a follow-on session wires them into the
                        // per-seed enumeration / fine-sync pipeline
                        // described in spec ref
                        // `research/specs/spec-wsjtr-cross-sequence-a7.md`
                        // §5-10. Inert by default.
                        if cross_seq_enabled {
                            // The next slot's parity is the opposite of the
                            // current window's parity (we treat the current
                            // window as "slot N+1" for the look-up).
                            let now_slot_start =
                                window_received_utc - chrono::Duration::seconds(13);
                            let current_parity =
                                pancetta_core::slot::SlotParity::of(now_slot_start);
                            let opposite_parity: u8 = match current_parity {
                                pancetta_core::slot::SlotParity::Even => 1,
                                pancetta_core::slot::SlotParity::Odd => 0,
                            };
                            if let Ok(cache_guard) = cross_sequence_cache.read() {
                                let seeds = cache_guard.get_a7_candidates_with_parity(
                                    std::time::SystemTime::now(),
                                    pancetta_qso::CROSS_SEQUENCE_DEFAULT_MAX_AGE_SLOTS,
                                    opposite_parity,
                                );
                                if !seeds.is_empty() {
                                    debug!(
                                        target: "hb237",
                                        "cross-sequence A7: {} prior-slot opposite-parity seeds available (parity={}); per-seed enumeration not yet wired",
                                        seeds.len(),
                                        opposite_parity,
                                    );
                                }
                            }
                        }

                        info!("FT8 decoder: received window ({} samples)", window.len());

                        // Generate waterfall data
                        let audio_f64: Vec<f64> = window.iter().map(|&s| s as f64).collect();
                        match decoder.generate_waterfall_data(&audio_f64) {
                            Ok(wf) => {
                                let range = wf.max_power - wf.min_power;
                                info!(
                                    "Waterfall: {}x{} matrix, power range {:.1}..{:.1} dB",
                                    wf.power_matrix.len(),
                                    wf.power_matrix.first().map(|r| r.len()).unwrap_or(0),
                                    wf.min_power,
                                    wf.max_power,
                                );
                                let rows: Vec<Vec<f32>> = if range > 0.0 {
                                    wf.power_matrix
                                        .iter()
                                        .map(|row| {
                                            row.iter()
                                                .map(|&p| ((p - wf.min_power) / range) as f32)
                                                .collect()
                                        })
                                        .collect()
                                } else {
                                    wf.power_matrix
                                        .iter()
                                        .map(|row| vec![0.0f32; row.len()])
                                        .collect()
                                };
                                let _ = waterfall_tx.send(rows.clone());
                                if let Some(ref auto_wf_tx) = self_waterfall_to_auto_tx {
                                    let _ = auto_wf_tx.try_send(rows);
                                }
                            }
                            Err(e) => {
                                warn!("Waterfall generation error: {}", e);
                            }
                        }

                        // hb-091 scoped fast-path: when activeQso is set
                        // and scoped_fast_path is enabled (hb-216 S2: set
                        // by the tier probe on Moderate/Slow hardware, or
                        // by env var PANCETTA_SCOPED_FAST_PATH=1 as
                        // operator override), run a scoped Costas search
                        // at the partner's freq_bin BEFORE the standard
                        // pipeline. ~3× faster wall-clock (p99 866ms vs
                        // 2332ms on M4 reference); reliably completes
                        // inside the 2s slot budget so the QSO state
                        // machine advances before the next slot's TX
                        // boundary. Standard pipeline still runs after as
                        // the authoritative result; the QSO state machine
                        // deduplicates by verifying from_station ==
                        // expected DX callsign per is_message_relevant.
                        const SCOPED_HALF_WIDTH: usize = 5;
                        let scoped_fast_path_enabled = scoped_fast_path.load(Ordering::Relaxed);
                        let scoped_decodes: Vec<pancetta_ft8::DecodedMessage> =
                            if scoped_fast_path_enabled {
                                let partner_freq_hz =
                                    active_qso_freq_hz.read().ok().and_then(|g| *g);
                                if let Some(freq_hz) = partner_freq_hz {
                                    let center = (freq_hz / 6.25).round() as usize;
                                    let lo = center.saturating_sub(SCOPED_HALF_WIDTH);
                                    let hi = center.saturating_add(SCOPED_HALF_WIDTH);
                                    decoder
                                        .decode_window_scoped(&window, lo..=hi)
                                        .unwrap_or_default()
                                } else {
                                    Vec::new()
                                }
                            } else {
                                Vec::new()
                            };

                        // Tag scoped decodes with slot parity (same
                        // derivation as the standard pipeline below) and
                        // fire them at the QSO state machine immediately.
                        // The QSO state machine handles duplicates of
                        // already-consumed messages by rejecting them at
                        // is_message_relevant (state has already advanced).
                        if !scoped_decodes.is_empty() {
                            let scoped_slot_start =
                                window_received_utc - chrono::Duration::seconds(13);
                            let scoped_parity =
                                pancetta_core::slot::SlotParity::of(scoped_slot_start);
                            for mut decoded_msg in scoped_decodes {
                                decoded_msg.slot_parity = Some(scoped_parity);
                                info!(
                                    "FT8 scoped fast-path: {} (SNR: {:.0}, freq: {:.1})",
                                    decoded_msg.text,
                                    decoded_msg.snr_db,
                                    decoded_msg.frequency_offset
                                );
                                let qso_msg = ComponentMessage::new(
                                    ComponentId::Ft8Decoder,
                                    ComponentId::Qso,
                                    MessageType::DecodedMessage(decoded_msg),
                                    Instant::now(),
                                );
                                let bus = message_bus.clone();
                                rt.spawn(async move {
                                    if let Err(e) = bus.send_message(qso_msg).await {
                                        debug!(
                                            "Failed to forward scoped fast-path decode to QSO: {}",
                                            e
                                        );
                                    }
                                });
                            }
                        }

                        // Primary decoder: ft8_lib (reference C implementation)
                        // with full sliding-frame spectrogram — matches WSJT-X sensitivity
                        let ft8lib_messages =
                            pancetta_ft8::Ft8Decoder::decode_window_ft8lib(&window);

                        // Secondary: our native decoder with AP enhancement
                        let current_qso_ap =
                            active_qso_ap.read().ok().and_then(|guard| guard.clone());
                        let ap_context = pancetta_ft8::ApContext {
                            my_call: my_call_ap.clone(),
                            recent_calls: recent_pool.clone(),
                            active_qso: current_qso_ap,
                        };

                        // hb-229: QSO partner band-collapse. When a QSO is
                        // active and the operator hasn't overridden via env
                        // var, narrow the Costas sweep to ±60 Hz around the
                        // partner's audio freq. The pure observer in
                        // `qso_filter` maps Option<freq_hz> → Option<range>;
                        // the FT8 layer's `decode_window_with_ap_scoped`
                        // is the existing hb-091 hook that clamps the
                        // sync sweep to the supplied bin range.
                        //
                        // hb-230: paired with band-collapse, expose the
                        // partner audio freq to the decoder so the
                        // relaxed-sync-threshold branch fires inside the
                        // narrow window. Same QSO-filter override gates
                        // both signals (the two mechanisms compose; an
                        // operator who wants wide decode also wants the
                        // standard sync threshold).
                        let partner_freq_for_main = active_qso_freq_hz.read().ok().and_then(|g| *g);
                        let narrow_filter_bins =
                            super::qso_filter::compute_narrow_filter_bins_default(
                                partner_freq_for_main,
                                qso_filter_override_off,
                            );
                        let partner_freq_for_relaxed_sync =
                            super::qso_filter::partner_freq_for_relaxed_sync(
                                partner_freq_for_main,
                                qso_filter_override_off,
                            );
                        if let Some(ref range) = narrow_filter_bins {
                            debug!(
                                "hb-229: narrowing main decode to freq_bins {}..={} (partner {:.1} Hz)",
                                range.start(),
                                range.end(),
                                partner_freq_for_main.unwrap_or(0.0),
                            );
                        }
                        let native_messages = decoder
                            .decode_window_with_ap_scoped_partner(
                                &window,
                                &ap_context,
                                narrow_filter_bins,
                                partner_freq_for_relaxed_sync,
                            )
                            .unwrap_or_default();

                        // Merge: start with ft8_lib results, add any native-only
                        // decodes (e.g. from AP injection) that ft8_lib missed
                        let mut seen_texts: std::collections::HashSet<String> =
                            ft8lib_messages.iter().map(|m| m.text.clone()).collect();
                        let mut decoded_messages = ft8lib_messages;
                        for msg in native_messages {
                            if seen_texts.insert(msg.text.clone()) {
                                decoded_messages.push(msg);
                            }
                        }

                        // Update decode timestamp
                        rt.block_on(async {
                            let mut timestamp = last_decode_timestamp.write().await;
                            *timestamp = Some(Instant::now());
                        });

                        health_total_decodes
                            .fetch_add(decoded_messages.len() as u64, Ordering::Relaxed);

                        info!(
                            "FT8 decoder: {} messages decoded ({} ft8lib + native merge)",
                            decoded_messages.len(),
                            decoded_messages.len()
                        );

                        // Window's audio came from the slot that started 13s before
                        // receipt; computing parity from the receipt timestamp keeps the
                        // tag invariant under decode latency. (next_slot_start would
                        // give the wrong slot if decode pushes us into the next slot
                        // before we tag.)
                        let slot_start = window_received_utc - chrono::Duration::seconds(13);
                        let window_parity = pancetta_core::slot::SlotParity::of(slot_start);

                        for decoded_msg in decoded_messages.iter_mut() {
                            decoded_msg.slot_parity = Some(window_parity);
                        }

                        // hb-062: apply FP filter post-decode, pre-broadcast.
                        // When fp_filter is None (default), all decodes pass
                        // through unchanged. When Some, decodes whose extracted
                        // callsigns don't appear in any reference source are
                        // dropped (logged at debug level).
                        if let Some(ref filter) = fp_filter {
                            let pre = decoded_messages.len();
                            decoded_messages.retain(|m| filter.accept(&m.text));
                            let dropped = pre - decoded_messages.len();
                            if dropped > 0 {
                                debug!("FP filter dropped {} of {} decodes", dropped, pre);
                            }
                        }

                        // Update shared cross-slot state (hb-048 / hb-057 /
                        // hb-173 substrate). Runs post-FP-filter so the three
                        // downstream tables never ingest decodes the continuity
                        // filter judged false. The container is SHIPPED-INFRA
                        // — no consumer reads from it yet; downstream
                        // hypotheses will hook in here in future sessions.
                        for decoded_msg in &decoded_messages {
                            let parity_u8 = decoded_msg.slot_parity.map(|p| match p {
                                pancetta_core::slot::SlotParity::Even => 0u8,
                                pancetta_core::slot::SlotParity::Odd => 1u8,
                            });
                            cross_time_state.record_decode(&pancetta_qso::DecodeRecord {
                                from_callsign: decoded_msg.message.from_callsign.clone(),
                                to_callsign: decoded_msg.message.to_callsign.clone(),
                                text: decoded_msg.text.clone(),
                                frequency_hz: decoded_msg.frequency_offset,
                                time_offset_s: decoded_msg.time_offset,
                                slot_parity: parity_u8,
                                at: decoded_msg.timestamp,
                            });

                            // hb-237: cross-sequence A7 cache populate.
                            // Only when the master flag is on, only for
                            // decodes with a sender callsign and parity
                            // tag, and only via the trust-gated insert
                            // (FP-amplification mitigation; see hb-237
                            // spec §"FP risk"). The trust filter is
                            // shared with hb-062; when the filter is
                            // absent we still admit on the assumption
                            // that the post-FP-filter loop position
                            // already filtered (the trust-gate is an
                            // additional defense, not the only one).
                            if cross_seq_enabled {
                                if let (Some(ref call), Some(parity)) =
                                    (&decoded_msg.message.from_callsign, parity_u8)
                                {
                                    if let Ok(mut cache_guard) = cross_sequence_cache.write() {
                                        let admitted =
                                            if let Some(ref filter) = cross_sequence_fp_filter {
                                                cache_guard.record_decoded_trusted(
                                                    call,
                                                    decoded_msg.frequency_offset,
                                                    parity,
                                                    decoded_msg.timestamp,
                                                    filter,
                                                )
                                            } else {
                                                cache_guard.record_decoded(
                                                    call,
                                                    decoded_msg.frequency_offset,
                                                    parity,
                                                    decoded_msg.timestamp,
                                                );
                                                true
                                            };
                                        if !admitted {
                                            debug!(
                                                target: "hb237",
                                                "cross-sequence A7: callsign {} not in trust set; not seeded",
                                                call,
                                            );
                                        }
                                    }
                                }
                            }
                        }

                        for decoded_msg in &decoded_messages {
                            info!(
                                "FT8 decoded: {} (SNR: {:.0}, freq: {:.1})",
                                decoded_msg.text, decoded_msg.snr_db, decoded_msg.frequency_offset
                            );

                            // Send to TUI via point-to-point channel
                            if ft8_to_tui_tx.send(decoded_msg.clone()).is_err() {
                                warn!("TUI channel disconnected");
                            }

                            // Forward to other components via message bus (fire-and-forget
                            // to avoid stalling the decoder thread with block_on)
                            let auto_msg = ComponentMessage::new(
                                ComponentId::Ft8Decoder,
                                ComponentId::Autonomous,
                                MessageType::DecodedMessage(decoded_msg.clone()),
                                Instant::now(),
                            );
                            let bus1 = message_bus.clone();
                            rt.spawn(async move {
                                if let Err(e) = bus1.send_message(auto_msg).await {
                                    debug!(
                                        "Failed to forward decoded message to Autonomous: {}",
                                        e
                                    );
                                }
                            });

                            let qso_msg = ComponentMessage::new(
                                ComponentId::Ft8Decoder,
                                ComponentId::Qso,
                                MessageType::DecodedMessage(decoded_msg.clone()),
                                Instant::now(),
                            );
                            let bus2 = message_bus.clone();
                            rt.spawn(async move {
                                if let Err(e) = bus2.send_message(qso_msg).await {
                                    debug!("Failed to forward decoded message to QSO: {}", e);
                                }
                            });

                            let psk_msg = ComponentMessage::new(
                                ComponentId::Ft8Decoder,
                                ComponentId::PskReporter,
                                MessageType::DecodedMessage(decoded_msg.clone()),
                                Instant::now(),
                            );
                            let bus3 = message_bus.clone();
                            rt.spawn(async move {
                                if let Err(e) = bus3.send_message(psk_msg).await {
                                    debug!(
                                        "Failed to forward decoded message to PSKReporter: {}",
                                        e
                                    );
                                }
                            });
                        }

                        // Update AP recent_pool with newly decoded callsigns
                        for msg in &decoded_messages {
                            if let Some(ref call) = msg.message.from_callsign {
                                if !recent_pool.iter().any(|r| r.callsign == *call) {
                                    if let Some(ap) =
                                        pancetta_ft8::RecentCallAp::new(call, msg.snr_db)
                                    {
                                        recent_pool.push(ap);
                                    }
                                }
                            }
                        }
                        // Keep strongest 20, prune weak entries
                        recent_pool.sort_by(|a, b| {
                            b.last_snr
                                .partial_cmp(&a.last_snr)
                                .unwrap_or(std::cmp::Ordering::Equal)
                        });
                        recent_pool.truncate(20);
                    }
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    Err(crossbeam_channel::RecvTimeoutError::Disconnected) => {
                        info!("FT8 decoder: input channel disconnected");
                        break;
                    }
                }
            }

            info!("FT8 component stopped");
            Ok(())
        });

        self.named_task_handles
            .push((ComponentId::Ft8Decoder, handle));
        info!("FT8 component started");
        Ok(())
    }
}
