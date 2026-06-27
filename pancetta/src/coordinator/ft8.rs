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
use tracing::{debug, error, info, span, warn, Level};

use crate::message_bus::{ComponentId, ComponentMessage, MessageType};

/// Cumulative count of decode-window panics caught (and skipped) by the
/// `catch_unwind` guards in the FT8 hot loop. A non-zero, growing value means
/// pathological windows are being skipped to keep the station on-air — surfaced
/// in the log (target `ft8.decode`). The OS supervisor (docs/RUNBOOK.md) is the
/// backstop for faults that cannot unwind (e.g. a native ft8_lib C abort).
static DECODE_PANIC_COUNT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Maximum length (in chars) for any human-facing decoded string field.
/// FT8 message payloads are short (callsigns ≤ ~11 chars, grids 4-6, the
/// full text well under 64); anything longer is malformed/hostile.
const MAX_DECODED_FIELD_LEN: usize = 64;

/// I-16: sanitize a human-facing decoded string before it crosses the
/// message-bus boundary into the TUI / QSO state machine / ADIF log.
///
/// A decoded FT8 callsign / grid / text that carries an embedded control
/// character or ANSI escape sequence could corrupt TUI rendering or
/// log/ADIF output. The decoder's `is_plausible` / `looks_like_callsign`
/// checks cover most malformed input, but this is a defensive
/// belt-and-suspenders strip applied once, at the boundary:
///   - drops control chars (`< 0x20`), DEL (`0x7f`), and ESC (`0x1b`),
///   - caps length to [`MAX_DECODED_FIELD_LEN`] chars.
fn sanitize_decoded_field(s: &str) -> String {
    s.chars()
        .filter(|&c| c != '\u{1b}' && c != '\u{7f}' && c >= '\u{20}')
        .take(MAX_DECODED_FIELD_LEN)
        .collect()
}

/// I-16: sanitize every human-facing string field on a [`pancetta_ft8::DecodedMessage`]
/// in place — applied once at the bus boundary before the message is broadcast.
/// Covers the top-level `text` plus the inner `message`'s `from_callsign`,
/// `to_callsign`, `grid_square`, and `text` (all the operator-/log-visible strings).
fn sanitize_decoded_message(decoded_msg: &mut pancetta_ft8::DecodedMessage) {
    decoded_msg.text = sanitize_decoded_field(&decoded_msg.text);
    if let Some(ref call) = decoded_msg.message.from_callsign {
        decoded_msg.message.from_callsign = Some(sanitize_decoded_field(call));
    }
    if let Some(ref call) = decoded_msg.message.to_callsign {
        decoded_msg.message.to_callsign = Some(sanitize_decoded_field(call));
    }
    if let Some(ref grid) = decoded_msg.message.grid_square {
        decoded_msg.message.grid_square = Some(sanitize_decoded_field(grid));
    }
    if let Some(ref text) = decoded_msg.message.text {
        decoded_msg.message.text = Some(sanitize_decoded_field(text));
    }
}

/// hb-237 Session 3 — pure helper: translate the pancetta-qso
/// [`pancetta_qso::A7SeedEntry`] cache entries into the decoder's ABI-
/// stable [`pancetta_ft8::CrossSequenceSeed`] inputs.
///
/// The two types are deliberately decoupled: `A7SeedEntry` lives in
/// pancetta-qso (which depends on pancetta-ft8), so pancetta-ft8 cannot
/// see it. The coordinator owns the translation at the invocation
/// boundary. See `research/specs/spec-wsjtr-cross-sequence-a7.md`
/// §"State lives in pancetta-qso, not pancetta-ft8".
///
/// Partner callsign is currently left `None`; the cache records only
/// the call1. A follow-on session can plumb call2 through.
pub(crate) fn a7_seeds_to_cross_sequence_seeds(
    seeds: &[pancetta_qso::A7SeedEntry],
) -> Vec<pancetta_ft8::CrossSequenceSeed> {
    seeds
        .iter()
        .map(|e| pancetta_ft8::CrossSequenceSeed {
            callsign: e.callsign.clone(),
            partner_callsign: None,
            freq_hz: e.freq_hz,
        })
        .collect()
}

/// hb-237 Session 3 — pure helper: invoke the cross-sequence consumer
/// and return the deduplicated subset of recovered decodes (those whose
/// text is not already in `seen_texts`). Mutates `seen_texts` to absorb
/// the newly added texts — keeping a single source of truth for what's
/// been emitted to downstream.
///
/// Returns `(new_decodes, recovered_count)`. When the flag is OFF or
/// seeds are empty, returns `(vec![], 0)` without touching the audio.
///
/// Inspired by spec ref `research/specs/spec-wsjtr-cross-sequence-a7.md`.
pub(crate) fn invoke_cross_sequence_consumer(
    decoder: &mut Ft8Decoder,
    cross_seq_enabled: bool,
    samples: &[f32],
    seeds: &[pancetta_qso::A7SeedEntry],
    seen_texts: &mut std::collections::HashSet<String>,
) -> (Vec<pancetta_ft8::DecodedMessage>, usize) {
    if !cross_seq_enabled || seeds.is_empty() {
        return (Vec::new(), 0);
    }
    let cs_seeds = a7_seeds_to_cross_sequence_seeds(seeds);
    match decoder.try_cross_sequence_decodes(samples, &cs_seeds) {
        Ok(extra) => {
            let mut out = Vec::new();
            let mut recovered = 0usize;
            for msg in extra {
                if seen_texts.insert(msg.text.clone()) {
                    recovered += 1;
                    out.push(msg);
                }
            }
            (out, recovered)
        }
        Err(e) => {
            warn!(
                target: "hb237",
                "cross-sequence A7 consumer error (continuing without): {}",
                e,
            );
            (Vec::new(), 0)
        }
    }
}

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
        let gateway_enabled = self.gateway_enabled.clone();
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

                        // hb-237 cross-sequence A7 — pre-decode seed read
                        // (Session 3 wiring).
                        //
                        // When `cross_sequence_a7_enabled` is true, look up
                        // the prior slot's opposite-parity callsigns from
                        // the cross-sequence cache. The seeds are kept in
                        // scope for invocation AFTER the main decode merge
                        // below; that's where the consumer
                        // `decoder.try_cross_sequence_decodes` runs.
                        // Inert by default (flag default-OFF).
                        //
                        // Inspired by spec ref
                        // `research/specs/spec-wsjtr-cross-sequence-a7.md`
                        // §1-§5 (state lifecycle + seed handoff).
                        let cross_seq_seeds: Vec<pancetta_qso::A7SeedEntry> = if cross_seq_enabled {
                            // The current window's parity (we treat the
                            // current window as "slot N+1" for the
                            // look-up — seeds are from slot N which is the
                            // opposite parity).
                            let now_slot_start =
                                window_received_utc - chrono::Duration::seconds(13);
                            let current_parity =
                                pancetta_core::slot::SlotParity::of(now_slot_start);
                            let opposite_parity: u8 = match current_parity {
                                pancetta_core::slot::SlotParity::Even => 1,
                                pancetta_core::slot::SlotParity::Odd => 0,
                            };
                            let seeds = cross_sequence_cache
                                .read()
                                .ok()
                                .map(|cache_guard| {
                                    cache_guard.get_a7_candidates_with_parity(
                                        std::time::SystemTime::now(),
                                        pancetta_qso::CROSS_SEQUENCE_DEFAULT_MAX_AGE_SLOTS,
                                        opposite_parity,
                                    )
                                })
                                .unwrap_or_default();
                            debug!(
                                target: "hb237",
                                "cross-sequence A7: {} prior-slot opposite-parity seeds available (parity={})",
                                seeds.len(),
                                opposite_parity,
                            );
                            seeds
                        } else {
                            Vec::new()
                        };

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
                                // I-16: sanitize at the bus boundary (scoped
                                // fast-path also broadcasts to TUI/QSO).
                                sanitize_decoded_message(&mut decoded_msg);
                                // Boundary-relative DT: the DSP window's sample 0
                                // sits at slot_boundary − WINDOW_LEAD_SECS, so the
                                // decoder's slice-relative time_offset overstates DT
                                // by exactly the lead. Subtract it so the reported DT
                                // is ≈0 for a station on the slot boundary.
                                decoded_msg.time_offset -= super::WINDOW_LEAD_SECS;
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
                        // with full sliding-frame spectrogram — matches WSJT-X sensitivity.
                        //
                        // Wrapped in catch_unwind so a Rust-side panic on one
                        // pathological window is logged + skipped (empty result)
                        // rather than aborting the whole station. Release builds
                        // use panic="unwind" for this. NOTE: a native abort
                        // inside the ft8_lib C code cannot unwind — the OS
                        // supervisor (docs/RUNBOOK.md) is the backstop for that.
                        let ft8lib_messages = std::panic::catch_unwind(
                            std::panic::AssertUnwindSafe(|| {
                                pancetta_ft8::Ft8Decoder::decode_window_ft8lib(&window)
                            }),
                        )
                        .unwrap_or_else(|_| {
                            let n = DECODE_PANIC_COUNT
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                + 1;
                            error!(
                                target: "ft8.decode",
                                "ft8_lib decode panicked on a window (#{n}) — skipping it; station continues"
                            );
                            Vec::new()
                        });

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
                        // Same catch_unwind resilience for the native AP decoder.
                        let native_messages = std::panic::catch_unwind(
                            std::panic::AssertUnwindSafe(|| {
                                decoder.decode_window_with_ap_scoped_partner(
                                    &window,
                                    &ap_context,
                                    narrow_filter_bins,
                                    partner_freq_for_relaxed_sync,
                                )
                            }),
                        )
                        .unwrap_or_else(|_| {
                            let n = DECODE_PANIC_COUNT
                                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                                + 1;
                            error!(
                                target: "ft8.decode",
                                "native AP decode panicked on a window (#{n}) — skipping it; station continues"
                            );
                            Ok(Vec::new())
                        })
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

                        // hb-237 cross-sequence A7 — consumer invocation
                        // (Session 3). Runs AFTER the main decode merge so
                        // the post-pass only attempts to recover decodes
                        // that the standard pipeline missed. The consumer
                        // itself defends-in-depth on
                        // `cross_sequence_a7_enabled` (returns Ok([]) when
                        // OFF) and on `seeds.is_empty()` (no-op).
                        //
                        // The consumer takes `CrossSequenceSeed` (the
                        // decoder's own ABI-stable seed type), not
                        // `A7SeedEntry` (the pancetta-qso cache entry) —
                        // we translate at this boundary. Per the hb-237
                        // spec §"State lives in pancetta-qso", the
                        // decoder is stateless across slots; the
                        // coordinator owns the translation.
                        //
                        // Partner callsign is left `None` in this session;
                        // the cache currently records only the call1.
                        // Without partner the consumer enumerates only
                        // single-callsign templates from the existing a7
                        // bank (see decoder §"Per-seed attempt — candidate
                        // enumeration"). A follow-on session can plumb
                        // call2 through the save filter.
                        //
                        // Inspired by spec ref
                        // `research/specs/spec-wsjtr-cross-sequence-a7.md`
                        // §4, §8-§11.
                        let (new_decodes, cross_seq_recovered) = invoke_cross_sequence_consumer(
                            &mut decoder,
                            cross_seq_enabled,
                            &window,
                            &cross_seq_seeds,
                            &mut seen_texts,
                        );
                        if cross_seq_enabled && !cross_seq_seeds.is_empty() {
                            debug!(
                                target: "hb237",
                                "cross-sequence A7: seeds={} recovered={} (after dedup)",
                                cross_seq_seeds.len(),
                                cross_seq_recovered,
                            );
                        }
                        decoded_messages.extend(new_decodes);

                        // Update decode timestamp (A9: lock-free atomic — no
                        // more rt.block_on on the decoder thread per window).
                        last_decode_timestamp
                            .store(super::now_epoch_ms(), std::sync::atomic::Ordering::Relaxed);

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
                            // I-16: strip control/ANSI chars and cap length on
                            // the human-facing string fields, once, at the bus
                            // boundary before any consumer (cross-slot state,
                            // TUI, QSO, PSKReporter, ADIF) sees them.
                            sanitize_decoded_message(decoded_msg);
                            // Boundary-relative DT correction (live path only). The
                            // DSP window is anchored so sample 0 = slot_boundary −
                            // WINDOW_LEAD_SECS; the decoder reports time_offset
                            // relative to sample 0, so subtracting the lead yields a
                            // DT that is ≈0 for a station transmitting on the slot
                            // boundary (was ≈ +2 s with the old last-15-s slice).
                            // Applied here, before any consumer (TUI delta_time,
                            // cross-slot state, autonomous time_offset_s, PSKReporter)
                            // reads decoded_msg.time_offset. The WAV-replay path
                            // (wav_playback.rs) has its own slot-aligned slicing and
                            // does NOT pass through this loop, so its DT is untouched.
                            decoded_msg.time_offset -= super::WINDOW_LEAD_SECS;
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

                            // Additive: also forward to the read-only remote
                            // gateway when enabled (gated — no clone/send when
                            // off). The existing →Tui/→Qso/→PskReporter sends
                            // above are untouched.
                            if gateway_enabled.load(Ordering::Relaxed) {
                                let gw_msg = ComponentMessage::new(
                                    ComponentId::Ft8Decoder,
                                    ComponentId::RemoteGateway,
                                    MessageType::DecodedMessage(decoded_msg.clone()),
                                    Instant::now(),
                                );
                                let bus4 = message_bus.clone();
                                rt.spawn(async move {
                                    if let Err(e) = bus4.send_message(gw_msg).await {
                                        debug!(
                                            "Failed to forward decoded message to RemoteGateway: {}",
                                            e
                                        );
                                    }
                                });
                            }
                        }

                        // Update AP recent_pool with newly decoded callsigns.
                        // I-6: cap the number of *new* unique calls we construct
                        // per slot. An air-attacker spamming many unique novel
                        // callsigns in one slot would otherwise force a
                        // `RecentCallAp::new()` construction per call (CPU
                        // pressure on the decoder thread) before the final
                        // `truncate(20)` runs. Short-circuit once enough new
                        // calls have been collected this slot; truncate(20) still
                        // applies below to keep the strongest entries.
                        const MAX_NEW_CALLS_PER_SLOT: usize = 50;
                        for msg in &decoded_messages {
                            if recent_pool.len() >= MAX_NEW_CALLS_PER_SLOT {
                                break;
                            }
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

// =============================================================================
// hb-237 Session 3 — coordinator-side cross-sequence A7 invocation tests
// =============================================================================
//
// The hot loop inside `start_ft8_pipeline` is a `spawn_blocking` thread that
// owns shared coordinator state and a real `Ft8Decoder` — exercising it
// directly is impractical. Instead these tests target the pure helpers
// extracted above: `a7_seeds_to_cross_sequence_seeds` (the boundary
// translation) and `invoke_cross_sequence_consumer` (the invocation
// wrapper). The helpers carry the same default-OFF guard and the same
// dedup semantics the hot loop uses.
//
// Inspired by spec ref `research/specs/spec-wsjtr-cross-sequence-a7.md`.

#[cfg(test)]
mod cross_sequence_invocation_tests {
    use super::{a7_seeds_to_cross_sequence_seeds, invoke_cross_sequence_consumer};
    use pancetta_ft8::{Ft8Config, Ft8Decoder, WINDOW_SAMPLES};
    use pancetta_qso::A7SeedEntry;
    use std::collections::HashSet;
    use std::time::SystemTime;

    fn make_seed(call: &str, freq_hz: f64) -> A7SeedEntry {
        A7SeedEntry {
            callsign: call.to_string(),
            freq_hz,
            slot_parity: 0,
            decoded_at: SystemTime::now(),
        }
    }

    /// Translation correctness: callsign and freq are preserved 1:1;
    /// partner is set to None in this session.
    #[test]
    fn seed_translation_preserves_callsign_and_freq() {
        let seeds = vec![make_seed("K1ABC", 1200.0), make_seed("W2XYZ", 1500.5)];
        let cs = a7_seeds_to_cross_sequence_seeds(&seeds);
        assert_eq!(cs.len(), 2);
        assert_eq!(cs[0].callsign, "K1ABC");
        assert_eq!(cs[0].freq_hz, 1200.0);
        assert!(cs[0].partner_callsign.is_none());
        assert_eq!(cs[1].callsign, "W2XYZ");
        assert_eq!(cs[1].freq_hz, 1500.5);
        assert!(cs[1].partner_callsign.is_none());
    }

    /// Default-OFF contract: even with a non-empty seed list and a
    /// non-empty audio buffer, the wrapper must return (vec![], 0)
    /// without invoking the decoder. We confirm by also asserting
    /// `seen_texts` is unchanged.
    #[test]
    fn default_off_returns_empty_without_invoking_decoder() {
        let cfg = Ft8Config::default();
        assert!(
            !cfg.cross_sequence_a7_enabled,
            "default config must keep cross-sequence A7 OFF"
        );
        let mut dec = Ft8Decoder::new(cfg).expect("decoder ctor");
        let samples = vec![0.0f32; WINDOW_SAMPLES];
        let seeds = vec![make_seed("K1ABC", 1200.0)];
        let mut seen = HashSet::new();
        seen.insert("already-emitted".to_string());

        let (new_decodes, recovered) = invoke_cross_sequence_consumer(
            &mut dec, /* enabled (coordinator-side gate) */ false, &samples, &seeds, &mut seen,
        );
        assert!(
            new_decodes.is_empty(),
            "default-OFF must produce no decodes"
        );
        assert_eq!(recovered, 0, "default-OFF must report 0 recovered");
        assert_eq!(
            seen.len(),
            1,
            "default-OFF must not perturb the seen-texts dedup set"
        );
    }

    /// Empty-seed no-op: coordinator-side flag ON but empty cache.
    /// Consumer's own empty-seed guard short-circuits.
    #[test]
    fn enabled_with_empty_seeds_is_noop() {
        let cfg = Ft8Config {
            cross_sequence_a7_enabled: true,
            ..Ft8Config::default()
        };
        let mut dec = Ft8Decoder::new(cfg).expect("decoder ctor");
        let samples = vec![0.0f32; WINDOW_SAMPLES];
        let seeds: Vec<A7SeedEntry> = Vec::new();
        let mut seen = HashSet::new();

        let (new_decodes, recovered) =
            invoke_cross_sequence_consumer(&mut dec, true, &samples, &seeds, &mut seen);
        assert!(new_decodes.is_empty(), "empty-seed must produce no decodes");
        assert_eq!(recovered, 0);
    }

    /// End-to-end: with the flag ON, a populated seed list, and a
    /// synthetic WAV containing a reply rooted at the seeded callsign,
    /// the wrapper must return at least one decode flagged with
    /// `via_cross_sequence_a7 = true`. This mirrors the decoder's own
    /// `seeded_consumer_emits_cross_sequence_provenance` test but
    /// exercises the coordinator-side wrapper end-to-end.
    ///
    /// Note: this session's coordinator translation passes
    /// `partner_callsign: None` (the cache only stores call1). The
    /// decoder's a7 template generator falls back to
    /// `A7_FALLBACK_CALLS = ["K1ABC", "W1AW", ...]` for the "other"
    /// party. The synthesized reply uses W1AW (a fallback callsign)
    /// so the templates match. A follow-on session can plumb call2
    /// through the cache to remove the fallback dependence.
    #[test]
    fn enabled_with_seeded_reply_recovers_via_cross_sequence() {
        let reply_text = "W1AW K1ABC 73";
        let mut encoder = pancetta_ft8::Ft8Encoder::new();
        let symbols = encoder.encode_message(reply_text, None).expect("encode");
        let mut modulator = pancetta_ft8::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        // Relax the a7 thresholds for the synthetic clean signal (see
        // the decoder-side seeded test's rationale).
        let cfg = Ft8Config {
            cross_sequence_a7_enabled: true,
            a7_snr7_threshold: 2.0,
            a7_snr7b_threshold: 1.05,
            ..Ft8Config::default()
        };
        let mut dec = Ft8Decoder::new(cfg).expect("decoder ctor");

        // Seed: K1ABC was decoded in the prior slot at 2000 Hz (base
        // 1500 + offset 500 from the modulator).
        let seeds = vec![make_seed("K1ABC", 2000.0)];
        let mut seen = HashSet::new();

        let (new_decodes, recovered) =
            invoke_cross_sequence_consumer(&mut dec, true, &tx, &seeds, &mut seen);
        assert!(
            !new_decodes.is_empty(),
            "wrapper should emit at least one decode for a seeded reply; recovered={}",
            recovered
        );
        // All recovered decodes must carry the provenance flag.
        for m in &new_decodes {
            assert!(
                m.via_cross_sequence_a7,
                "all wrapper-recovered decodes must have via_cross_sequence_a7=true; got {:?}",
                m.text
            );
        }
        let has_target = new_decodes
            .iter()
            .any(|m| m.text == reply_text && m.via_cross_sequence_a7);
        assert!(
            has_target,
            "wrapper should emit reply '{}' with via_cross_sequence_a7=true; got texts: {:?}",
            reply_text,
            new_decodes
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
        );
        // And `seen` must contain the recovered text now — proving the
        // dedup substrate was mutated.
        assert!(
            seen.contains(reply_text),
            "wrapper must update the seen-texts dedup set with recovered decodes"
        );
    }

    /// Dedup contract: a recovered decode whose text is already in
    /// `seen_texts` must NOT be re-emitted by the wrapper (the main
    /// pipeline already handled it). The recovered counter must
    /// reflect post-dedup additions only. Uses the same W1AW-fallback
    /// reply as the recovery test so it actually matches a template.
    #[test]
    fn dedup_skips_recovered_decodes_already_in_seen_set() {
        let reply_text = "W1AW K1ABC 73";
        let mut encoder = pancetta_ft8::Ft8Encoder::new();
        let symbols = encoder.encode_message(reply_text, None).expect("encode");
        let mut modulator = pancetta_ft8::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        let cfg = Ft8Config {
            cross_sequence_a7_enabled: true,
            a7_snr7_threshold: 2.0,
            a7_snr7b_threshold: 1.05,
            ..Ft8Config::default()
        };
        let mut dec = Ft8Decoder::new(cfg).expect("decoder ctor");
        let seeds = vec![make_seed("K1ABC", 2000.0)];

        // Pre-populate `seen` with the very text we expect to recover.
        // The consumer may emit OTHER templates the WAV also matches —
        // the dedup contract is per-text, not all-or-nothing — so we
        // only assert the seeded text is suppressed.
        let mut seen = HashSet::new();
        seen.insert(reply_text.to_string());

        let (new_decodes, _recovered) =
            invoke_cross_sequence_consumer(&mut dec, true, &tx, &seeds, &mut seen);
        let leaked = new_decodes.iter().any(|m| m.text == reply_text);
        assert!(
            !leaked,
            "dedup must suppress the specific text already in seen-set; new_decodes={:?}",
            new_decodes
                .iter()
                .map(|m| m.text.as_str())
                .collect::<Vec<_>>()
        );
    }
}

// =============================================================================
// I-16 — decoded-field sanitization at the message-bus boundary
// =============================================================================
#[cfg(test)]
mod sanitize_decoded_field_tests {
    use super::{sanitize_decoded_field, sanitize_decoded_message, MAX_DECODED_FIELD_LEN};
    use pancetta_ft8::{DecodedMessage, Ft8Message};

    #[test]
    fn strips_ansi_escape_sequence() {
        // A SGR color escape: ESC [ 3 1 m … ESC [ 0 m
        let hostile = "\u{1b}[31mK1ABC\u{1b}[0m";
        assert_eq!(sanitize_decoded_field(hostile), "[31mK1ABC[0m");
        // The raw ESC (0x1b) bytes are gone (the literal '[' / digits remain,
        // but they are inert text — the control byte that drives the terminal
        // is what we strip).
        assert!(!sanitize_decoded_field(hostile).contains('\u{1b}'));
    }

    #[test]
    fn strips_control_chars_and_del() {
        let hostile = "K1\u{0}A\u{7}B\nC\r\u{7f}";
        // NUL, BEL, LF, CR, DEL all dropped; printable chars survive.
        assert_eq!(sanitize_decoded_field(hostile), "K1ABC");
    }

    #[test]
    fn caps_over_long_string() {
        let long: String = "A".repeat(MAX_DECODED_FIELD_LEN + 50);
        let out = sanitize_decoded_field(&long);
        assert_eq!(out.chars().count(), MAX_DECODED_FIELD_LEN);
    }

    #[test]
    fn leaves_normal_callsign_unchanged() {
        assert_eq!(sanitize_decoded_field("K5ARH"), "K5ARH");
        assert_eq!(sanitize_decoded_field("EA8/G8BCG"), "EA8/G8BCG");
    }

    #[test]
    fn leaves_normal_grid_and_text_unchanged() {
        assert_eq!(sanitize_decoded_field("FN31"), "FN31");
        assert_eq!(sanitize_decoded_field("CQ K1ABC FN42"), "CQ K1ABC FN42");
    }

    #[test]
    fn sanitize_message_covers_all_string_fields() {
        let msg = Ft8Message {
            from_callsign: Some("K1\u{1b}ABC".to_string()),
            to_callsign: Some("W2\u{7}XYZ".to_string()),
            grid_square: Some("FN\u{0}31".to_string()),
            text: Some("hello\u{1b}[mworld".to_string()),
            ..Ft8Message::default()
        };

        let mut decoded = DecodedMessage::new(msg, -10.0, 1.0, 1200.0, 0.0);
        decoded.text = "CQ \u{1b}[31mK1ABC\u{7f}".to_string();

        sanitize_decoded_message(&mut decoded);

        assert_eq!(decoded.text, "CQ [31mK1ABC");
        assert_eq!(decoded.message.from_callsign.as_deref(), Some("K1ABC"));
        assert_eq!(decoded.message.to_callsign.as_deref(), Some("W2XYZ"));
        assert_eq!(decoded.message.grid_square.as_deref(), Some("FN31"));
        assert_eq!(decoded.message.text.as_deref(), Some("hello[mworld"));
        // No control / ESC / DEL bytes survive anywhere.
        for field in [
            decoded.text.as_str(),
            decoded.message.from_callsign.as_deref().unwrap_or(""),
            decoded.message.to_callsign.as_deref().unwrap_or(""),
            decoded.message.grid_square.as_deref().unwrap_or(""),
            decoded.message.text.as_deref().unwrap_or(""),
        ] {
            assert!(field
                .chars()
                .all(|c| c != '\u{1b}' && c != '\u{7f}' && c >= '\u{20}'));
        }
    }
}
