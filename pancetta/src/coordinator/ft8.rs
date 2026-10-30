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
use pancetta_ft8::merge::{drop_unresolved_hash_twins, merge_ft8lib_first, MergedDecodes};
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

/// Read-only accessor for the Layer 3 health panel (`coordinator/tui_relay.rs`).
pub(crate) fn decode_panic_count() -> u64 {
    DECODE_PANIC_COUNT.load(std::sync::atomic::Ordering::Relaxed)
}

/// Maximum length (in chars) for any human-facing decoded string field.
/// FT8 message payloads are short (callsigns ≤ ~11 chars, grids 4-6, the
/// full text well under 64); anything longer is malformed/hostile.
const MAX_DECODED_FIELD_LEN: usize = 64;

/// Ceiling on decode wall time so the DSP pipeline never backs up
/// (decoder-speed-overhaul Task 12).
///
/// FT8: window every 15 s, decode-phase at 13 s -> ceiling 2000 ms.
/// FT4: window every 7.5 s, decode-phase at 6.5 s -> ceiling 800 ms.
///
/// The boundary (`slot_ns == 7_500_000_000`, i.e. exactly FT4's period) is
/// inclusive on the FT4 (800 ms) side — a slot period at or below FT4's is
/// treated as the faster mode's tighter budget.
///
/// Scope: this ceiling (and the published per-window budget) only governs the
/// native `pancetta-ft8` AP-enhanced decode call. The separate
/// `decode_window_ft8lib_protocol` ft8_lib C FFI decode path (below) runs
/// unconditionally and is not bounded by either budget.
pub(crate) fn decode_budget_ceiling_ms(slot_ns: u64) -> u64 {
    if slot_ns <= 7_500_000_000 {
        800
    } else {
        2000
    }
}

/// The `Ft8Config` fields a live `Ft8Decoder` bakes in at construction.
/// When any of them changes, the hot loop rebuilds the decoder before it
/// uses the budget published with that change. Every field
/// `effort::apply_effort_overrides` writes must be here
/// (`every_effort_override_field_is_a_rebuild_trigger`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DecoderRebuildKey {
    pub(crate) max_decode_passes: usize,
    pub(crate) osd_depth: Option<u8>,
    pub(crate) protocol: pancetta_ft8::Protocol,
    pub(crate) time_varying_subtraction_enabled: bool,
    /// PAN-158: on for FT8 Eco at its 1 ms budget.
    pub(crate) fine_sync_enabled: bool,
    /// PAN-162/PAN-241: the FT8 impulse-robust LLR knee, as `f64::to_bits`
    /// (`f64` is not `Eq`).
    pub(crate) impulse_robust_llr_bits: Option<u64>,
}

impl DecoderRebuildKey {
    pub(crate) fn of(cfg: &pancetta_ft8::Ft8Config) -> Self {
        Self {
            max_decode_passes: cfg.max_decode_passes,
            osd_depth: cfg.osd_depth,
            protocol: cfg.protocol,
            time_varying_subtraction_enabled: cfg.time_varying_subtraction_enabled,
            fine_sync_enabled: cfg.fine_sync_enabled,
            impulse_robust_llr_bits: cfg.impulse_robust_llr.map(f64::to_bits),
        }
    }
}

/// Everything the hot loop uses for one window, all from one
/// `DecodeWindowConfig` publish (PAN-164).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct AppliedDecodeWindow {
    pub(crate) rebuild_key: DecoderRebuildKey,
    pub(crate) budget_ms: u64,
    pub(crate) cross_sequence_a7_enabled: bool,
}

impl AppliedDecodeWindow {
    pub(crate) fn of(window: &super::DecodeWindowConfig) -> Self {
        Self {
            rebuild_key: DecoderRebuildKey::of(window.ft8()),
            budget_ms: window.budget_ms(),
            cross_sequence_a7_enabled: window.ft8().cross_sequence_a7_enabled,
        }
    }

    /// `0` resolves to the ceiling of the protocol the live decoder was
    /// built for; a nonzero budget is clamped to it.
    pub(crate) fn effective_budget_ms(&self) -> u64 {
        let ceiling_ms =
            decode_budget_ceiling_ms(self.rebuild_key.protocol.slot_ns().max(0) as u64);
        if self.budget_ms == 0 {
            ceiling_ms
        } else {
            self.budget_ms.min(ceiling_ms)
        }
    }
}

/// Outcome of one [`refresh_decode_window`] call.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum DecodeWindowRefresh<E> {
    /// A writer holds the lock; `applied` is untouched.
    Busy,
    /// Same decoder fields; the new budget (and flags) adopted.
    Adopted,
    /// Decoder rebuilt from the new publish, which is now adopted whole.
    Rebuilt,
    /// Rebuild failed; the previous decoder and its budget stay paired.
    RebuildFailed(E),
}

/// The hot loop's one read of the shared decode-window state per window
/// (PAN-164). A single `try_read`; everything adopted comes from that guard.
/// When the published decoder fields differ from `applied`'s, `rebuild`
/// gets a clone of the published config (guard already dropped) and
/// `applied` adopts the publish only if it returns `Ok`.
pub(crate) fn refresh_decode_window<E>(
    shared: &tokio::sync::RwLock<super::DecodeWindowConfig>,
    applied: &mut AppliedDecodeWindow,
    rebuild: impl FnOnce(pancetta_ft8::Ft8Config) -> Result<(), E>,
) -> DecodeWindowRefresh<E> {
    let Ok(guard) = shared.try_read() else {
        return DecodeWindowRefresh::Busy;
    };
    let next = AppliedDecodeWindow::of(&guard);
    if next.rebuild_key == applied.rebuild_key {
        drop(guard);
        *applied = next;
        return DecodeWindowRefresh::Adopted;
    }
    let cfg = guard.ft8().clone();
    drop(guard);
    match rebuild(cfg) {
        Ok(()) => {
            *applied = next;
            DecodeWindowRefresh::Rebuilt
        }
        Err(e) => DecodeWindowRefresh::RebuildFailed(e),
    }
}

/// PAN-165: ms left on `budget` at `at` (`None` = unlimited).
fn headroom_ms(budget: &pancetta_ft8::DecodeBudget, at: Instant) -> Option<u64> {
    budget
        .deadline()
        .map(|d| d.saturating_duration_since(at).as_millis() as u64)
}

/// PAN-165: ms past `budget`'s deadline at `at` (0 when on time or unlimited).
fn over_deadline_ms(budget: &pancetta_ft8::DecodeBudget, at: Instant) -> u64 {
    budget
        .deadline()
        .map_or(0, |d| at.saturating_duration_since(d).as_millis() as u64)
}

/// Compute the active protocol's slot parity for a decoded window.
///
/// `received_utc` is the window's receipt timestamp (captured immediately on
/// recv, before any decode work — see the comment at the call site). The
/// slot start is recovered by subtracting `decode_phase` (how far past the
/// slot boundary the window arrives), then parity is derived over the given
/// `slot_ns` period.
///
/// Pulled out of the decode loop as a small pure function so a live mode
/// switch can be exercised in a unit test: this function has no memory of a
/// prior call, so feeding it freshly-read `decode_phase`/`slot_ns` values
/// every iteration (as the decode loop in `start_ft8_pipeline` does) can
/// never "stick" to a stale mode's period the way caching them once at
/// thread startup did.
fn slot_parity_for_receipt(
    received_utc: chrono::DateTime<chrono::Utc>,
    decode_phase: chrono::Duration,
    slot_ns: i64,
) -> pancetta_core::slot::SlotParity {
    let slot_start = received_utc - decode_phase;
    pancetta_core::slot::SlotParity::of_with_period(slot_start, slot_ns)
}

/// Translate a tagged decode into the shape [`pancetta_qso::CrossTimeState`]
/// records. `slot_ns` is the period this window's `slot_parity` was
/// computed on (the decode loop's per-iteration `slot_ns`).
///
/// PAN-146: carries the band and slot grid of the observation so the TX
/// worker's DX-parity gate can ignore a parity heard on another band or
/// mode. The dial comes from the message's own `captured_dial_hz` (PAN-67),
/// never a live re-read.
fn cross_time_decode_record(
    decoded_msg: &pancetta_ft8::DecodedMessage,
    slot_ns: i64,
) -> pancetta_qso::DecodeRecord {
    pancetta_qso::DecodeRecord {
        from_callsign: decoded_msg.message.from_callsign.clone(),
        to_callsign: decoded_msg.message.to_callsign.clone(),
        text: decoded_msg.text.clone(),
        frequency_hz: decoded_msg.frequency_offset,
        time_offset_s: decoded_msg.time_offset,
        slot_parity: decoded_msg.slot_parity.map(|p| match p {
            pancetta_core::slot::SlotParity::Even => 0u8,
            pancetta_core::slot::SlotParity::Odd => 1u8,
        }),
        at: decoded_msg.timestamp,
        captured_dial_hz: decoded_msg.captured_dial_hz.unwrap_or(0),
        slot_ns,
    }
}

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

/// `[decoder].ap_eval_mode` (2026-07-17 operator finding): split decoded
/// messages into (delivered, suppressed) by whether AP (a priori) injection
/// produced them (`ap_level > 0`).
///
/// AP decoding deliberately biases the LDPC solver toward finding the
/// operator's own callsign in a signal, so weak genuine calls decode — but
/// the same bias can converge on pure noise into a phantom "someone is
/// calling us" message (`pancetta-ft8::decoder`'s own comment: "AP
/// injection biases the LDPC solver toward our callsign, producing phantom
/// messages... from noise"). Observed live: other stations decoded calling
/// a `/P`-suffixed variant of the operator's callsign at very weak SNR
/// (-15 to -17 dB), which the always-answer-callers path (#39) then replied
/// to mid-QSO — calls that most likely were never actually made.
///
/// When `eval_mode` is `false`, this is a no-op: `(all messages, empty)`,
/// preserving behavior from before this flag existed. When `true`, AP
/// decodes (`ap_level > 0`) are pulled out of the delivered set entirely —
/// callers must still log the suppressed set (with `ap_level`) themselves,
/// but must NOT forward it to the TUI, QSO engine, cross-slot state, or any
/// other consumer, so a phantom AP decode can never trigger a reply or be
/// pounced on by the autonomous operator. Non-AP decodes (`ap_level == 0`)
/// are always delivered, in both modes.
fn partition_ap_eval_decodes(
    decoded_messages: Vec<pancetta_ft8::DecodedMessage>,
    eval_mode: bool,
) -> (
    Vec<pancetta_ft8::DecodedMessage>,
    Vec<pancetta_ft8::DecodedMessage>,
) {
    if !eval_mode {
        return (decoded_messages, Vec::new());
    }
    decoded_messages.into_iter().partition(|m| m.ap_level == 0)
}

#[cfg(test)]
mod ap_eval_mode_tests {
    use super::partition_ap_eval_decodes;
    use pancetta_ft8::{DecodedMessage, Ft8Message};

    fn decoded_with_ap(ap_level: u8, text: &str) -> DecodedMessage {
        let mut d = DecodedMessage::new(Ft8Message::default(), -10.0, 0.5, 1300.0, 0.1);
        d.ap_level = ap_level;
        d.text = text.to_string();
        d
    }

    #[test]
    fn eval_mode_off_is_a_no_op_regardless_of_ap_level() {
        let messages = vec![
            decoded_with_ap(0, "CQ K1ABC FN42"),
            decoded_with_ap(2, "K5ARH/P VA3TS PO59"),
            decoded_with_ap(4, "K5ARH/P NW2M R OE18"),
        ];
        let (delivered, suppressed) = partition_ap_eval_decodes(messages.clone(), false);
        assert_eq!(
            delivered.len(),
            3,
            "eval_mode=false must deliver everything"
        );
        assert!(
            suppressed.is_empty(),
            "eval_mode=false must suppress nothing"
        );
        // Order and content preserved, not just count.
        assert_eq!(delivered[0].text, messages[0].text);
        assert_eq!(delivered[1].text, messages[1].text);
        assert_eq!(delivered[2].text, messages[2].text);
    }

    #[test]
    fn eval_mode_on_suppresses_only_ap_decodes() {
        let messages = vec![
            decoded_with_ap(0, "CQ K1ABC FN42"),
            decoded_with_ap(2, "K5ARH/P VA3TS PO59"),
            decoded_with_ap(0, "W2XYZ K3DEF -05"),
            decoded_with_ap(4, "K5ARH/P NW2M R OE18"),
        ];
        let (delivered, suppressed) = partition_ap_eval_decodes(messages, true);
        assert_eq!(delivered.len(), 2, "only ap_level==0 decodes are delivered");
        assert!(delivered.iter().all(|m| m.ap_level == 0));
        assert_eq!(delivered[0].text, "CQ K1ABC FN42");
        assert_eq!(delivered[1].text, "W2XYZ K3DEF -05");

        assert_eq!(suppressed.len(), 2, "all ap_level>0 decodes are suppressed");
        assert!(suppressed.iter().all(|m| m.ap_level > 0));
        assert_eq!(suppressed[0].text, "K5ARH/P VA3TS PO59");
        assert_eq!(suppressed[0].ap_level, 2);
        assert_eq!(suppressed[1].text, "K5ARH/P NW2M R OE18");
        assert_eq!(suppressed[1].ap_level, 4);
    }

    #[test]
    fn eval_mode_on_all_non_ap_delivers_everything() {
        let messages = vec![decoded_with_ap(0, "A"), decoded_with_ap(0, "B")];
        let (delivered, suppressed) = partition_ap_eval_decodes(messages, true);
        assert_eq!(delivered.len(), 2);
        assert!(suppressed.is_empty());
    }

    #[test]
    fn eval_mode_on_all_ap_suppresses_everything() {
        let messages = vec![decoded_with_ap(1, "A"), decoded_with_ap(3, "B")];
        let (delivered, suppressed) = partition_ap_eval_decodes(messages, true);
        assert!(delivered.is_empty());
        assert_eq!(suppressed.len(), 2);
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
    pub(crate) async fn start_ft8_pipeline(&mut self) -> Result<()> {
        let (ft8_rx, ft8_to_tui_tx, waterfall_tx, health_total_decodes) = {
            let h = self.decode_handles()?;
            (
                h.dsp_to_ft8_rx.clone(),
                h.ft8_to_tui_tx.clone(),
                h.waterfall_tx.clone(),
                h.health_total_decodes.clone(),
            )
        };
        let span = span!(Level::INFO, "start_ft8");
        let _enter = span.enter();

        info!("Starting FT8 component");

        // hb-216 S2 / PAN-164: read the shared decode window (budget +
        // Ft8Config, published together). The tier probe, the TUI effort
        // cycle and a mode switch may republish it; the hot loop re-reads
        // it once per window and rebuilds the decoder when a
        // `DecoderRebuildKey` field changed.
        let initial_window = self.decode_window.read().await.clone();
        let mut decoder = Ft8Decoder::new(initial_window.ft8().clone())?;
        let decode_window_shared = self.decode_window.clone();

        // hb-216 S2: scoped fast-path activation flag. Seeded from env at
        // startup; rewritten by the tier probe (Moderate/Slow → true).
        let scoped_fast_path = self.scoped_fast_path.clone();

        let shutdown = self.shutdown_signal.clone();
        let last_decode_timestamp = self.last_decode_timestamp.clone();
        let message_bus = self.message_bus.clone();
        // Snapshot TX interval evidence at window receipt.
        let last_ptt_on_ms = self.last_ptt_on_ms.clone();
        let last_ptt_off_ms = self.last_ptt_off_ms.clone();
        let desense_ptt_active = self.ptt_active.clone();
        let display_feed_enabled = self.display_feed_enabled.clone();
        let wsjtx_enabled = self.wsjtx_enabled.clone();
        let self_waterfall_to_auto_tx = self.waterfall_to_auto_tx.clone();

        // Read station callsign for AP decoding before moving into the thread.
        // Also resolve the Task W5.3 window lead-in from the SAME config read
        // dsp.rs uses (`resolve_window_lead_secs`) so the DT correction below
        // always matches the lead dsp.rs actually anchored the window to.
        // `ap_eval_mode` is read once here too — like the lead-in, it's a
        // static-for-the-session decode-pipeline knob, not hot-reloaded
        // per-window (see `partition_ap_eval_decodes`).
        // Task 5 step 4 (gap 2/4, docs/ap-decoding-design.md §1): priority
        // weights for ranking `recent_calls` (Ap2 candidates) by
        // `PriorityScorer::evaluate_cq` instead of raw SNR — read once here
        // (weights don't hot-reload) alongside the other one-time reads
        // above, same pattern `tui_relay.rs`'s display scorer uses.
        let (station_callsign, window_lead_secs, ap_eval_mode, recent_calls_priority_weights) = {
            let config = self.config.read().await;
            let p = &config.autonomous.priorities;
            (
                config.station.callsign.clone(),
                super::resolve_window_lead_secs(&config.decoder),
                config.decoder.ap_eval_mode,
                pancetta_qso::priority::PriorityWeights {
                    needed_dxcc: p.needed_dxcc,
                    needed_grid: p.needed_grid,
                    pota_sota: p.pota_sota,
                    rarity: p.rarity,
                    signal_strength: p.signal_strength,
                    duplicate_penalty: p.duplicate_penalty,
                    recent_failure_penalty: p.recent_failure_penalty,
                    atno_bonus: p.atno_bonus,
                },
            )
        };
        // PAN-17 round 2 (Codex review #248, finding 2): seed our own
        // callsign into this decoder's i3=4 hash table so a compound-call
        // DX replying to us resolves our callsign back to plain text
        // instead of the unresolvable "<...>" placeholder — see
        // `Ft8Decoder::seed_hash_callsign`'s doc for why this is required
        // (not just polish) for a compound-callsign QSO to ever advance
        // past our opening call.
        decoder.seed_hash_callsign(&station_callsign);
        // Same Arc<CachedStationLookup> the QSO component's active-QSO
        // ranking and the TUI's display scorer read — an in-memory
        // RwLock-backed snapshot the decoder thread reads via its own
        // `.clone()`, so it always sees the latest needed/rarity/worked
        // data without a new data path.
        let recent_calls_lookup = self.cached_lookup.clone();

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

        // hb-062: shared FP filter (Arc<RwLock<Option<Arc<...>>>>). Cloning
        // the Arc here shares the SAME lock the coordinator later writes the
        // production filter into (see the comment on the `fp_filter` field
        // in `mod.rs`) — the thread must read it fresh each window rather
        // than snapshot it now, since it's still `None` at this point.
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

        // Active protocol slot length (FT8 → 15e9, FT4 → 7.5e9). Re-read from
        // this shared atomic once per decode-loop iteration (below, inside the
        // `while` loop) so a live mode switch (Shift+M) is picked up promptly
        // rather than only at thread startup. Read by the parity-stamping
        // sites via `SlotParity::of_with_period`. mode=FT8 is byte-identical
        // to the prior `SlotParity::of` (which hardcodes 15e9).
        let active_slot_ns = self.active_slot_ns.clone();

        // Active protocol decode phase (FT8 → 13e9, FT4 → 6.5e9 ns): how far
        // past the slot boundary the window is received. Subtracted below to
        // recover the slot start before parity-stamping. Also re-read once per
        // iteration alongside `active_slot_ns` for the same reason. mode=FT8
        // is 13e9 ns, byte-identical to the prior hardcoded
        // `Duration::seconds(13)`.
        let active_decode_phase_ns = self.active_decode_phase_ns.clone();

        // decoder-speed-overhaul Task 12: decode-budget metrics.
        // `decode_last_elapsed_ms`/`decode_last_budget_exhausted` are written
        // after each window's budgeted decode calls and read by the TUI
        // relay's periodic `PipelineHealth` push. The budget itself comes
        // from `decode_window_shared`, read once per window (PAN-164).
        let decode_last_elapsed_ms = self.decode_last_elapsed_ms.clone();
        let decode_last_budget_exhausted = self.decode_last_budget_exhausted.clone();

        // Run FT8 decoder on a dedicated thread to avoid tokio starvation
        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            info!("FT8 decoder thread started");

            // hb-216 S2 / PAN-164: the decode-window publish the current
            // decoder was built from, with its budget. Written only by
            // `refresh_decode_window`, so the decoder fields and the
            // budget always come from the same publish.
            let mut applied_window = AppliedDecodeWindow::of(&initial_window);

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

            // Task 5 step 4: built once per decoder-thread lifetime (weights
            // are a one-time read; the lookup Arc's internal RwLock always
            // reflects the latest worked/needed/rarity data). Used below to
            // rank `recent_pool` by `evaluate_cq` instead of raw SNR.
            //
            // KNOWN LIMITATION: `RecentCallAp` carries only a callsign +
            // last-heard SNR, no grid or real RF dial frequency (only an
            // audio offset, wrong scale for band classification). `grid =
            // None` is passed below, so `is_needed_grid`/`is_grid_needed_on_
            // band` never fire for this pool (both need `Some(grid)` to do
            // anything) — safely inert, not corrupting.
            //
            // `freq_hz = 0.0` is also passed (no real dial frequency to
            // give). Review fix (AP-decoding Task 5): `freq_hz = 0.0` used
            // to land in `CachedStationLookup`'s synthetic "0MHZ" band
            // bucket, which — for `is_dxcc_needed_on_band` specifically —
            // does NOT default to "not needed": a band key that's never
            // populated (true for "0MHZ", since no real QSO is ever logged
            // on it) reads as "needed" (`!worked.get(&band).is_some_and(..)`
            // when absent = true). That silently forced `needed_dxcc`
            // (the largest weight in `score_cq_detailed`) to `true` for
            // nearly every candidate, regardless of whether the entity was
            // genuinely needed. Fixed at the `is_dxcc_needed_on_band` impl
            // (`priority_evaluator.rs`): `freq_hz <= 0.0` now short-circuits
            // to `false` there, so `needed_dxcc` correctly falls back to the
            // reliable global `is_needed_dxcc` signal alone for this pool.
            // Net effect: global-scope signals (ATNO, global needed-DXCC,
            // rarity, POTA/SOTA pattern, signal strength) rank correctly;
            // band-scoped bonuses (needed-on-this-band, duplicate-on-this-
            // band, needed-grid-on-this-band) are inert (not corrupting)
            // here. Full per-band accuracy is used for the (higher-stakes)
            // concurrent-QSO ranking in qso.rs, which has real state/
            // metadata frequency data available.
            let recent_calls_scorer = pancetta_qso::PriorityScorer::new(
                recent_calls_priority_weights,
                Box::new((*recent_calls_lookup).clone()),
            );

            let mut desense = super::desense::DesenseDetector::default();

            while !shutdown.load(Ordering::Acquire) {
                match ft8_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                    Ok(super::pipeline::DecodeWindow {
                        samples: window,
                        dial_hz,
                    }) => {
                        // Capture receipt time immediately — before any decode
                        // work — so parity tagging is invariant under decode
                        // latency. (If we captured now() after decode, a slow
                        // slot on a loaded MiniPC could push us into slot N+1
                        // and produce the wrong parity, causing the autonomous
                        // operator to TX in the same slot as the DX.)
                        let window_received_utc = chrono::Utc::now();
                        let receipt_ptt_on = last_ptt_on_ms.load(Ordering::Acquire);
                        let receipt_ptt_off = last_ptt_off_ms.load(Ordering::Acquire);
                        let receipt_ptt_active = desense_ptt_active.load(Ordering::Acquire);
                        // decoder-speed-overhaul Task 12: wall-clock anchor for
                        // this window's decode deadline(s). Same receipt event
                        // as `window_received_utc` above, just captured as a
                        // monotonic `Instant` since `DecodeBudget` deadlines
                        // are `Instant`-based (not calendar time).
                        let window_ready_at = std::time::Instant::now();
                        // decoder-speed-overhaul Task 12: accumulated across
                        // whichever budgeted decode call(s) run this window
                        // (the scoped fast-path only runs conditionally; the
                        // primary native decode always runs) and reported once
                        // at the end of the window. It counts only the
                        // two decode calls; the pre-native breakdown
                        // (waterfall, scoped, ft8_lib, headroom left when the
                        // native call starts) is in the `window timing` line
                        // instead (PAN-165).
                        let mut window_decode_elapsed_ms: u64 = 0;
                        let mut window_decode_budget_exhausted = false;
                        // PAN-165: scoped fast-path time for this window's
                        // `window timing` line (0 when it did not run).
                        let mut window_scoped_ms: u64 = 0;

                        // Re-read the active protocol slot length/decode phase
                        // once per iteration (not once at thread startup) so a
                        // live mode switch (Shift+M -> try_switch_operating_mode
                        // updating these atomics) is reflected in this window's
                        // parity stamp. Stamping stale-period parity forever
                        // after a switch would let the QSO state machine latch
                        // the wrong tx_parity -> on-air collision, the exact
                        // failure half-duplex parity scheduling exists to
                        // prevent. mode=FT8 is byte-identical to before (same
                        // constant value read every iteration instead of once).
                        let slot_ns = active_slot_ns.load(Ordering::Relaxed);
                        let decode_phase = chrono::Duration::nanoseconds(
                            active_decode_phase_ns.load(Ordering::Relaxed),
                        );

                        // PAN-164: one read of the shared decode-window
                        // state per window. Budget, decoder fields and flags
                        // all come from the same publish; on contention (a
                        // writer holds the lock — hb-216 S2's "skip and pick
                        // it up next window") or a failed rebuild, the whole
                        // previous window state (decoder included) is kept.
                        let refresh = refresh_decode_window(
                            &decode_window_shared,
                            &mut applied_window,
                            |cfg| {
                                let (passes, osd, protocol, fine_sync) = (
                                    cfg.max_decode_passes,
                                    cfg.osd_depth,
                                    cfg.protocol,
                                    cfg.fine_sync_enabled,
                                );
                                let mut d = Ft8Decoder::new(cfg)?;
                                info!(
                                    "FT8 decoder rebuilt: max_decode_passes={}, osd_depth={:?}, protocol={}, fine_sync_enabled={}",
                                    passes, osd, protocol, fine_sync
                                );
                                // PAN-17 round 2: a rebuild is a fresh
                                // Ft8Decoder with an empty hash table —
                                // re-seed our own callsign or a compound-call
                                // DX's reply silently stops resolving after
                                // the next tier/protocol config change.
                                d.seed_hash_callsign(&station_callsign);
                                // PAN-27 finding 2: also carry over every
                                // OTHER caller this decoder instance had
                                // already learned (the round-2/round-4
                                // seeding loops below) — otherwise a
                                // compound operator learned earlier this
                                // session decodes as unresolved again
                                // immediately after a rebuild, even though
                                // the hash had already resolved once.
                                for call in decoder.learned_callsigns() {
                                    d.seed_hash_callsign(&call);
                                }
                                decoder = d;
                                Ok::<(), pancetta_ft8::Ft8Error>(())
                            },
                        );
                        if let DecodeWindowRefresh::RebuildFailed(e) = refresh {
                            warn!(
                                "FT8 decoder rebuild failed (keeping previous decoder and its budget): {}",
                                e
                            );
                        }
                        // The protocol the live decoder was built for.
                        let last_protocol = applied_window.rebuild_key.protocol;
                        // hb-237: cross-sequence A7 enable flag, from the
                        // same publish.
                        let cross_seq_enabled = applied_window.cross_sequence_a7_enabled;
                        // decoder-speed-overhaul Task 12: mode-aware decode
                        // deadline for this window. A `0` budget falls back
                        // to the ceiling of the live decoder's own protocol;
                        // a nonzero operator/preset value is clamped to it.
                        // Shared by both decode call sites below so a single
                        // window has one consistent deadline.
                        let decode_budget = pancetta_ft8::DecodeBudget::until(
                            window_ready_at
                                + std::time::Duration::from_millis(
                                    applied_window.effective_budget_ms(),
                                ),
                        );

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
                            let current_parity =
                                slot_parity_for_receipt(window_received_utc, decode_phase, slot_ns);
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

                        // PAN-165: covers generation, normalisation and the
                        // channel sends below, all time the native call loses.
                        let waterfall_start = Instant::now();
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

                                // Additive: also forward the RAW (pre-
                                // normalization) rows to the read-only remote
                                // gateway for the `spectrum` serverEvent
                                // (dispensa Q-0024), gated exactly like the
                                // DecodedMessage relay below — no clone/send
                                // when off. The →Tui `waterfall_tx`/
                                // `self_waterfall_to_auto_tx` sends above
                                // (0-1 normalized) are untouched.
                                if display_feed_enabled.load(Ordering::Relaxed) {
                                    let bin_width_hz = if wf.frequency_bins.len() >= 2 {
                                        wf.frequency_bins[1] - wf.frequency_bins[0]
                                    } else {
                                        0.0
                                    };
                                    let audio_bin_start_hz =
                                        wf.frequency_bins.first().copied().unwrap_or(0.0);
                                    for (row, &t_offset) in
                                        wf.power_matrix.iter().zip(wf.time_bins.iter())
                                    {
                                        let mags_db: Vec<f32> =
                                            row.iter().map(|&v| v as f32).collect();
                                        let timestamp = window_received_utc
                                            + chrono::Duration::milliseconds(
                                                (t_offset * 1000.0) as i64,
                                            );
                                        let gw_msg = ComponentMessage::new(
                                            ComponentId::Ft8Decoder,
                                            ComponentId::RemoteGateway,
                                            MessageType::SpectrumRow {
                                                audio_bin_start_hz,
                                                bin_width_hz,
                                                mags_db,
                                                timestamp,
                                            },
                                            Instant::now(),
                                        );
                                        let bus_spectrum = message_bus.clone();
                                        rt.spawn(async move {
                                            if let Err(e) =
                                                bus_spectrum.send_message(gw_msg).await
                                            {
                                                debug!(
                                                    "Failed to forward spectrum row to RemoteGateway: {}",
                                                    e
                                                );
                                            }
                                        });
                                    }
                                }
                            }
                            Err(e) => {
                                warn!("Waterfall generation error: {}", e);
                            }
                        }
                        let waterfall_ms = waterfall_start.elapsed().as_millis() as u64;

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
                        //
                        // `[decoder].ap_eval_mode` does NOT gate this path:
                        // it dispatches straight to the QSO component below,
                        // before `partition_ap_eval_decodes` runs on the
                        // standard pipeline's output. This is safe today
                        // only because it always calls with
                        // `ApContext::default()` (`my_call: None`), which
                        // structurally disables the own-callsign AP hypotheses
                        // (AP1-4) that the eval mode exists to gate — see
                        // `decode_window_with_ap_scoped_partner_budgeted`'s
                        // `my_call.is_some()` guard. If this path ever starts
                        // passing a real `ApContext`, it must also route
                        // through `partition_ap_eval_decodes` (or be dropped
                        // when `ap_eval_mode` is on).
                        const SCOPED_HALF_WIDTH: usize = 5;
                        let scoped_fast_path_enabled = scoped_fast_path.load(Ordering::Relaxed);
                        let scoped_decodes: Vec<pancetta_ft8::DecodedMessage> =
                            if scoped_fast_path_enabled {
                                // PAN-72 round-8 redesign (fix 2): `.0` is the
                                // primary hint (this scoped path's original
                                // single-frequency meaning, unchanged); `.1`
                                // is the still-in-grace SECONDARY hint — the
                                // offset an unanswered `CallingCq` relocated
                                // away from. Union both windows so a caller
                                // answering EITHER offset is decoded by this
                                // fast path too, not just the slower
                                // authoritative main decode below.
                                //
                                // PAN-72 round-9 fix (Codex round 9, finding
                                // 1): the secondary now carries its own
                                // `expires_at`, re-checked HERE against
                                // `Utc::now()` on every read -- the old code
                                // trusted whatever was last written, so a
                                // secondary whose grace had genuinely lapsed
                                // (with no further StateChanged/
                                // TxOffsetApplied event to refresh it) kept
                                // widening this scoped window forever.
                                #[allow(clippy::type_complexity)]
                                let partner_freq_pair: Option<(
                                    f64,
                                    Option<(f64, chrono::DateTime<chrono::Utc>)>,
                                )> = active_qso_freq_hz.read().ok().and_then(|g| *g);
                                if let Some((freq_hz, secondary_freq_pair)) = partner_freq_pair {
                                    let center = (freq_hz / 6.25).round() as usize;
                                    let mut lo = center.saturating_sub(SCOPED_HALF_WIDTH);
                                    let mut hi = center.saturating_add(SCOPED_HALF_WIDTH);
                                    let live_secondary_hz = secondary_freq_pair.and_then(
                                        |(secondary_hz, expires_at)| {
                                            (chrono::Utc::now() < expires_at)
                                                .then_some(secondary_hz)
                                        },
                                    );
                                    if let Some(secondary_hz) = live_secondary_hz {
                                        let secondary_center =
                                            (secondary_hz / 6.25).round() as usize;
                                        lo = lo.min(
                                            secondary_center.saturating_sub(SCOPED_HALF_WIDTH),
                                        );
                                        hi = hi.max(
                                            secondary_center.saturating_add(SCOPED_HALF_WIDTH),
                                        );
                                    }
                                    let scoped_call_start = Instant::now();
                                    let (messages, report) = decoder
                                        .decode_window_with_ap_scoped_partner_budgeted(
                                            &window,
                                            &pancetta_ft8::ApContext::default(),
                                            Some(lo..=hi),
                                            None,
                                            decode_budget,
                                        )
                                        .unwrap_or_default();
                                    let scoped_elapsed_ms =
                                        scoped_call_start.elapsed().as_millis() as u64;
                                    window_decode_elapsed_ms += scoped_elapsed_ms;
                                    window_scoped_ms = scoped_elapsed_ms;
                                    window_decode_budget_exhausted |= report.budget_exhausted;
                                    debug!(
                                        target: "decode.budget",
                                        "scoped fast-path: elapsed={}ms exhausted={}",
                                        scoped_elapsed_ms,
                                        report.budget_exhausted,
                                    );
                                    messages
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
                            let scoped_parity =
                                slot_parity_for_receipt(window_received_utc, decode_phase, slot_ns);
                            for mut decoded_msg in scoped_decodes {
                                decoded_msg.slot_parity = Some(scoped_parity);
                                // I-16: sanitize at the bus boundary (scoped
                                // fast-path also broadcasts to TUI/QSO).
                                sanitize_decoded_message(&mut decoded_msg);
                                // Boundary-relative DT: the DSP window's sample 0
                                // sits at slot_boundary − window_lead_secs (Task
                                // W5.3: resolved once above from
                                // `[decoder].extended_capture_window_enabled`,
                                // defaulting to WINDOW_LEAD_SECS), so the
                                // decoder's slice-relative time_offset overstates DT
                                // by exactly the lead. Subtract it so the reported DT
                                // is ≈0 for a station on the slot boundary.
                                decoded_msg.time_offset -= window_lead_secs;
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
                        // Protocol-aware (2026-07-06): the vendored C library
                        // already supports FT4 natively, it was just never
                        // asked to — `decode_window_ft8lib_protocol` passes
                        // `last_protocol` (kept in sync with the live config
                        // above) instead of hardcoding FT8. Measured ~25x
                        // faster than the native Rust decoder, which matters
                        // most for FT4: its 7.5s slot leaves far less decode
                        // margin than FT8's 15s, and decode wall-clock time
                        // was found to be truncating FT4 transmissions (see
                        // the coalesce-window scaling fix earlier this week).
                        //
                        // Wrapped in catch_unwind so a Rust-side panic on one
                        // pathological window is logged + skipped (empty result)
                        // rather than aborting the whole station. Release builds
                        // use panic="unwind" for this. NOTE: a native abort
                        // inside the ft8_lib C code cannot unwind — the OS
                        // supervisor (docs/RUNBOOK.md) is the backstop for that.
                        let ft8lib_start = Instant::now();
                        let ft8lib_messages = std::panic::catch_unwind(
                            std::panic::AssertUnwindSafe(|| {
                                pancetta_ft8::Ft8Decoder::decode_window_ft8lib_protocol(
                                    &window,
                                    last_protocol,
                                )
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
                        let ft8lib_ms = ft8lib_start.elapsed().as_millis() as u64;

                        // Secondary: our native decoder with AP enhancement.
                        //
                        // Task 5 (gap 2/4): `active_qso_ap` now holds the
                        // QSO component's ranked+capped list of ALL
                        // currently-active QSOs (highest `PriorityScorer`
                        // score first — see qso.rs's `rank_active_qsos_for_ap`).
                        // Both `active_qsos` (the ranked list, consumed by
                        // Ap5) and the back-compat singular `active_qso`
                        // (Ap3/Ap4) are derived from the SAME read, so the
                        // two can never disagree (`active_qso` is always
                        // `active_qsos.first().cloned()`).
                        let current_qsos: Vec<pancetta_ft8::QsoAp> = active_qso_ap
                            .read()
                            .ok()
                            .map(|guard| guard.clone())
                            .unwrap_or_default();
                        let ap_context = pancetta_ft8::ApContext {
                            my_call: my_call_ap.clone(),
                            recent_calls: recent_pool.clone(),
                            active_qso: current_qsos.first().cloned(),
                            active_qsos: current_qsos,
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
                        // PAN-72 round-8 redesign (fix 2): `.0` is the
                        // primary hint (this site's original single-frequency
                        // meaning, unchanged); `.1` is the still-in-grace
                        // SECONDARY hint. `compute_narrow_filter_bins_default_dual`
                        // is byte-identical to `compute_narrow_filter_bins_default`
                        // when the secondary is `None` (every case except an
                        // unanswered `CallingCq` still inside its relocation
                        // grace) — see `secondary_decoder_hint_freq_for` in
                        // `coordinator::qso`.
                        //
                        // PAN-72 round-9 fix (Codex round 9, finding 1): the
                        // secondary now carries its own `expires_at`
                        // alongside the frequency, so
                        // `compute_narrow_filter_bins_default_dual_with_expiry`
                        // re-evaluates freshness against `Utc::now()` on
                        // EVERY read (this decode loop runs every slot
                        // regardless of events) instead of trusting a value
                        // latched once at the last `StateChanged`/
                        // `TxOffsetApplied` write -- the bug being fixed is
                        // exactly that a secondary whose grace lapsed with
                        // no further event kept widening this filter
                        // forever.
                        #[allow(clippy::type_complexity)]
                        let qso_freq_pair: Option<(
                            f64,
                            Option<(f64, chrono::DateTime<chrono::Utc>)>,
                        )> = active_qso_freq_hz.read().ok().and_then(|g| *g);
                        let partner_freq_for_main = qso_freq_pair.map(|(primary, _)| primary);
                        let secondary_for_main = qso_freq_pair.and_then(|(_, secondary)| secondary);
                        let narrow_filter_bins =
                            super::qso_filter::compute_narrow_filter_bins_default_dual_with_expiry(
                                partner_freq_for_main,
                                secondary_for_main,
                                chrono::Utc::now(),
                                qso_filter_override_off,
                            );
                        let partner_freq_for_relaxed_sync =
                            super::qso_filter::partner_freq_for_relaxed_sync(
                                partner_freq_for_main,
                                qso_filter_override_off,
                            );
                        if let Some(ref range) = narrow_filter_bins {
                            info!(
                                "hb-229: narrowing main decode to freq_bins {}..={} (partner {:.1} Hz)",
                                range.start(),
                                range.end(),
                                partner_freq_for_main.unwrap_or(0.0),
                            );
                        }
                        // Same catch_unwind resilience for the native AP decoder.
                        // decoder-speed-overhaul Task 12: this is the primary
                        // decode call site — always runs (unlike the
                        // conditional scoped fast-path above), so it carries
                        // the representative per-window budget telemetry.
                        let native_call_start = Instant::now();
                        // PAN-165: production's deadline is anchored at window
                        // receipt, so this is what the native call really has.
                        let pre_native_ms = native_call_start
                            .duration_since(window_ready_at)
                            .as_millis() as u64;
                        let headroom_at_native = headroom_ms(&decode_budget, native_call_start);
                        let (native_messages, native_budget_report) = std::panic::catch_unwind(
                            std::panic::AssertUnwindSafe(|| {
                                decoder.decode_window_with_ap_scoped_partner_budgeted(
                                    &window,
                                    &ap_context,
                                    narrow_filter_bins,
                                    partner_freq_for_relaxed_sync,
                                    decode_budget,
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
                            Ok((Vec::new(), pancetta_ft8::DecodeBudgetReport::default()))
                        })
                        .unwrap_or_default();
                        let native_elapsed_ms = native_call_start.elapsed().as_millis() as u64;
                        window_decode_elapsed_ms += native_elapsed_ms;
                        window_decode_budget_exhausted |= native_budget_report.budget_exhausted;
                        debug!(
                            target: "decode.budget",
                            "primary decode: elapsed={}ms exhausted={} stages={:?}",
                            native_elapsed_ms,
                            native_budget_report.budget_exhausted,
                            native_budget_report.stages,
                        );

                        // Merge: start with ft8_lib results, add any native-only
                        // decodes (e.g. from AP injection) that ft8_lib missed.
                        // PAN-110: shared with the research harness so the
                        // published production-pipeline benchmark scores this
                        // exact merge.
                        let MergedDecodes {
                            messages: mut decoded_messages,
                            mut seen_texts,
                            ft8lib_count,
                            native_added: native_added_count,
                        } = merge_ft8lib_first(ft8lib_messages, native_messages);

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

                        // decoder-speed-overhaul Task 12: per-window budget
                        // summary (both decode call sites combined) — logged
                        // and forwarded to the TUI's periodic PipelineHealth
                        // metrics push (extends the existing decode-metrics
                        // path minimally; see `PipelineHealth` in
                        // pancetta-tui).
                        decode_last_elapsed_ms.store(window_decode_elapsed_ms, Ordering::Relaxed);
                        decode_last_budget_exhausted
                            .store(window_decode_budget_exhausted, Ordering::Relaxed);
                        debug!(
                            target: "decode.budget",
                            "window total: elapsed={}ms exhausted={}",
                            window_decode_elapsed_ms,
                            window_decode_budget_exhausted,
                        );
                        let window_end = Instant::now();
                        debug!(
                            target: "decode.budget",
                            "window timing: waterfall={}ms scoped={}ms ft8lib={}ms pre_native={}ms headroom_at_native={} native={}ms passes_run={} total={}ms over_deadline={}ms",
                            waterfall_ms,
                            window_scoped_ms,
                            ft8lib_ms,
                            pre_native_ms,
                            headroom_at_native.map_or("unlimited".to_string(), |h| format!("{h}ms")),
                            native_elapsed_ms,
                            native_budget_report.passes_run,
                            window_end.duration_since(window_ready_at).as_millis(),
                            over_deadline_ms(&decode_budget, window_end),
                        );

                        health_total_decodes
                            .fetch_add(decoded_messages.len() as u64, Ordering::Relaxed);

                        info!(
                            "FT8 decoder: {} messages decoded ({} ft8lib + {} native + {} cross-seq)",
                            decoded_messages.len(),
                            ft8lib_count,
                            native_added_count,
                            cross_seq_recovered,
                        );

                        let observation = super::desense::WindowObservation {
                            slot_start_ms: (window_received_utc - decode_phase)
                                .timestamp_millis()
                                .max(0) as u64,
                            slot_len_ms: (slot_ns / 1_000_000).max(1) as u64,
                            decoded_count: decoded_messages.len(),
                            band: pancetta_core::Band::from_frequency(dial_hz),
                            last_ptt_on_ms: receipt_ptt_on,
                            last_ptt_off_ms: receipt_ptt_off,
                            ptt_active_at_receipt: receipt_ptt_active,
                        };
                        let warning = desense.observe(&observation);
                        let (class, baseline_n, baseline_mean) = desense.summary(&observation);
                        debug!(target: "ft8.desense.window",
                            "band={} slot_start_ms={} count={} class={} baseline_n={} baseline_mean={:.1}",
                            observation.band.map_or_else(|| "unknown".to_owned(), |b| b.to_string()),
                            observation.slot_start_ms, observation.decoded_count, class.as_str(), baseline_n, baseline_mean);
                        if let Some(warning_text) = warning {
                            warn!(target: "ft8.desense", "{}", warning_text);
                            let diag_msg = ComponentMessage::new(
                                ComponentId::Ft8Decoder,
                                ComponentId::Tui,
                                MessageType::DiagnosticEvent {
                                    target: "ft8.desense",
                                    level: pancetta_core::DiagnosticLevel::Warn,
                                    text: warning_text,
                                    qso_id: None,
                                    callsign: None,
                                },
                                Instant::now(),
                            );
                            let bus_desense = message_bus.clone();
                            rt.spawn(async move {
                                let _ = bus_desense.send_message(diag_msg).await;
                            });
                        }

                        // Window's audio came from the slot that started 13s before
                        // receipt; computing parity from the receipt timestamp keeps the
                        // tag invariant under decode latency. (next_slot_start would
                        // give the wrong slot if decode pushes us into the next slot
                        // before we tag.)
                        let window_parity =
                            slot_parity_for_receipt(window_received_utc, decode_phase, slot_ns);

                        for decoded_msg in decoded_messages.iter_mut() {
                            decoded_msg.slot_parity = Some(window_parity);
                            // PAN-67 review round 2: stamp the dial frequency
                            // THIS window's audio was captured on (same value
                            // sent to the TUI relay below) onto the message
                            // itself, so every bus consumer (PSKReporter,
                            // autonomous, QSO) has it available too, not just
                            // the TUI channel.
                            decoded_msg.captured_dial_hz = Some(dial_hz);
                            // I-16: strip control/ANSI chars and cap length on
                            // the human-facing string fields, once, at the bus
                            // boundary before any consumer (cross-slot state,
                            // TUI, QSO, PSKReporter, ADIF) sees them.
                            sanitize_decoded_message(decoded_msg);
                            // Boundary-relative DT correction (live path only). The
                            // DSP window is anchored so sample 0 = slot_boundary −
                            // window_lead_secs (Task W5.3: resolved once above,
                            // defaults to WINDOW_LEAD_SECS); the decoder reports
                            // time_offset relative to sample 0, so subtracting the
                            // lead yields a DT that is ≈0 for a station transmitting
                            // on the slot boundary (was ≈ +2 s with the old
                            // last-15-s slice).
                            // Applied here, before any consumer (TUI delta_time,
                            // cross-slot state, autonomous time_offset_s, PSKReporter)
                            // reads decoded_msg.time_offset. The WAV-replay path
                            // (wav_playback.rs) has its own slot-aligned slicing and
                            // does NOT pass through this loop, so its DT is untouched.
                            decoded_msg.time_offset -= window_lead_secs;
                        }

                        // hb-062: apply FP filter post-decode, pre-broadcast.
                        // Read fresh each window rather than a value captured
                        // at thread-spawn time — the production filter is
                        // built later in `run()` and written into the SAME
                        // lock (see the `fp_filter` field comment in
                        // `mod.rs`). When the snapshot is None (default, or
                        // not yet built), all decodes pass through unchanged.
                        let fp_filter_snapshot = fp_filter.read().unwrap().clone();
                        if let Some(ref filter) = fp_filter_snapshot {
                            let pre = decoded_messages.len();
                            // 2026-07-17 operator finding: capture the raw,
                            // pre-filter texts before `retain` narrows
                            // `decoded_messages` down to only the accepted
                            // subset. `note_window_raw_calls` needs the
                            // FULL raw set (below) so a genuinely new,
                            // repeating station that gets rejected THIS
                            // window can still gain continuity for the
                            // NEXT one — without it, a solo unpaired novel
                            // callsign (no static/cqdx/rolling anchor) was
                            // rejected on every window forever, since
                            // `accept()` only ever recorded callsigns from
                            // decodes it had already accepted. See the
                            // `observed` field doc on `CallsignContinuityFilter`.
                            let raw_texts: Vec<String> =
                                decoded_messages.iter().map(|m| m.text.clone()).collect();
                            decoded_messages.retain(|m| filter.accept(&m.text));
                            let dropped = pre - decoded_messages.len();
                            if dropped > 0 {
                                // Was debug! (invisible at the default info
                                // level) — bumped to info! so the operator
                                // can actually see how much the FP filter
                                // is suppressing, since this is exactly the
                                // number that was invisible while diagnosing
                                // the 2026-07-17 "far fewer decodes than
                                // expected" report.
                                info!("FP filter dropped {} of {} decodes", dropped, pre);
                            }
                            filter.note_window_raw_calls(&raw_texts);
                        }

                        // `[decoder].ap_eval_mode`: pull AP-derived decodes
                        // (ap_level > 0) out of the delivered set entirely —
                        // before ANY downstream consumer (cross-slot state,
                        // TUI, QSO engine, autonomous) can see them, so a
                        // phantom AP decode can never trigger a reply. Every
                        // suppressed decode is still logged with its
                        // ap_level so the false-positive rate stays visible
                        // while this is under investigation. No-op (and
                        // zero suppressed) when ap_eval_mode is off.
                        let (decoded_messages, suppressed_ap_decodes) =
                            partition_ap_eval_decodes(decoded_messages, ap_eval_mode);
                        for msg in &suppressed_ap_decodes {
                            info!(
                                "AP eval-mode: decode suppressed (ap_level={}, text='{}', SNR: {:.0}, freq: {:.1})",
                                msg.ap_level, msg.text, msg.snr_db, msg.frequency_offset
                            );
                        }

                        // `[decoder].ap_eval_mode`: pull AP-derived decodes
                        // (ap_level > 0) out of the delivered set entirely —
                        // before ANY downstream consumer (cross-slot state,
                        // TUI, QSO engine, autonomous) can see them, so a
                        // phantom AP decode can never trigger a reply. Every
                        // suppressed decode is still logged with its
                        // ap_level so the false-positive rate stays visible
                        // while this is under investigation. No-op (and
                        // zero suppressed) when ap_eval_mode is off.
                        let (decoded_messages, suppressed_ap_decodes) =
                            partition_ap_eval_decodes(decoded_messages, ap_eval_mode);
                        for msg in &suppressed_ap_decodes {
                            info!(
                                "AP eval-mode: decode suppressed (ap_level={}, text='{}', SNR: {:.0}, freq: {:.1})",
                                msg.ap_level, msg.text, msg.snr_db, msg.frequency_offset
                            );
                        }

                        // PAN-40: the ft8lib+native merge above dedupes only
                        // by *exact* decode text. When the two backends
                        // decode the same physical signal differently — one
                        // resolving an i3=4 hash-callsign render, one
                        // leaving it the unresolved "<...>" placeholder —
                        // the texts differ, so both survive the merge and
                        // both would get forwarded downstream, where
                        // `callsigns_match` correctly (but uselessly)
                        // rejects the unresolved twin every cycle — silently
                        // starving an active QSO of every reply from its
                        // partner until the watchdog times it out. See
                        // `drop_unresolved_hash_twins` for the full
                        // rationale; it can only ever *remove* a decode
                        // that's a confident duplicate of one we're keeping,
                        // never drop a decode with no surviving twin.
                        //
                        // PAN-40 round-2 review (Codex finding 1): this MUST
                        // run here, after `partition_ap_eval_decodes` above
                        // — not before — so it operates only on the
                        // already-AP-vetted "delivered" stream and can never
                        // let an AP-derived resolved decode outrank (and
                        // cause the removal of) the non-AP copy that
                        // `[decoder].ap_eval_mode` guarantees always reaches
                        // consumers. See `drop_unresolved_hash_twins`'s doc
                        // comment for the full ordering rationale.
                        //
                        // `last_protocol` is passed through so the freq/DT
                        // tolerances are correctly scaled for whichever
                        // protocol (FT8 or FT4) this window actually decoded
                        // (PAN-40 round-3 review finding 3).
                        let (decoded_messages, hash_twins_dropped) =
                            drop_unresolved_hash_twins(decoded_messages, last_protocol);
                        if hash_twins_dropped > 0 {
                            debug!(
                                target: "ft8.decode",
                                "PAN-40: dropped {} unresolved-hash-placeholder twin(s) post AP-partition",
                                hash_twins_dropped,
                            );
                        }

                        // Update shared cross-slot state (hb-048 / hb-057 /
                        // hb-173 substrate). Runs post-FP-filter so the three
                        // downstream tables never ingest decodes the continuity
                        // filter judged false. The decoder-side hypotheses
                        // don't read these tables yet; the TX worker's PAN-141
                        // DX-parity gate reads `a7_recent_calls`, scoped by
                        // the band and slot grid each record carries
                        // (PAN-146).
                        //
                        // Same live-read rationale as `fp_filter_snapshot`
                        // above — one read per window, not per message.
                        let cross_sequence_fp_filter_snapshot =
                            cross_sequence_fp_filter.read().unwrap().clone();
                        for decoded_msg in &decoded_messages {
                            let parity_u8 = decoded_msg.slot_parity.map(|p| match p {
                                pancetta_core::slot::SlotParity::Even => 0u8,
                                pancetta_core::slot::SlotParity::Odd => 1u8,
                            });
                            cross_time_state
                                .record_decode(&cross_time_decode_record(decoded_msg, slot_ns));

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
                                        let admitted = if let Some(ref filter) =
                                            cross_sequence_fp_filter_snapshot
                                        {
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
                                "FT8 decoded: {} (SNR: {:.0}, freq: {:.1}, ap={})",
                                decoded_msg.text,
                                decoded_msg.snr_db,
                                decoded_msg.frequency_offset,
                                decoded_msg.ap_level
                            );

                            // Send to TUI via point-to-point channel, tagged with the
                            // dial frequency THIS window's audio was captured on (PAN-67:
                            // never a live re-read at relay time, which races an
                            // in-flight decode against a since-happened band switch).
                            if ft8_to_tui_tx
                                .send(super::pipeline::RelayedDecode {
                                    message: decoded_msg.clone(),
                                    dial_hz,
                                })
                                .is_err()
                            {
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
                            if display_feed_enabled.load(Ordering::Relaxed) {
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

                            // Additive: also forward to the WSJT-X UDP
                            // companion-protocol component when enabled
                            // (gated — no clone/send when off), exact
                            // structural mirror of the RemoteGateway block
                            // above. The →Tui/→Qso/→PskReporter/
                            // →RemoteGateway sends above are untouched.
                            if wsjtx_enabled.load(Ordering::Relaxed) {
                                let wsjtx_msg = ComponentMessage::new(
                                    ComponentId::Ft8Decoder,
                                    ComponentId::WsjtxUdp,
                                    MessageType::DecodedMessage(decoded_msg.clone()),
                                    Instant::now(),
                                );
                                let bus5 = message_bus.clone();
                                rt.spawn(async move {
                                    if let Err(e) = bus5.send_message(wsjtx_msg).await {
                                        debug!(
                                            "Failed to forward decoded message to WsjtxUdp: {}",
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
                        // Task 5 step 4: rank by PriorityScorer::evaluate_cq
                        // instead of raw SNR (docs/ap-decoding-design.md §1
                        // — "priority-ordering + a cap cuts Ap2 trials on
                        // low-value calls"), re-ordering the SAME 20-entry
                        // pool. No new config knob: the design's own wording
                        // only calls out today's 20-entry cap being reused,
                        // not replaced, so `max_ap_hypotheses`/`max_ap_qsos`
                        // are left untouched here (see the scorer
                        // construction comment above for the grid/freq
                        // limitation).
                        {
                            use pancetta_qso::DxEvaluator;
                            // Score each entry once (not per-comparison) —
                            // `evaluate_cq` does real lookup-table work.
                            let mut scored: Vec<(f64, pancetta_ft8::RecentCallAp)> = recent_pool
                                .drain(..)
                                .map(|call| {
                                    let score = recent_calls_scorer.evaluate_cq(
                                        &call.callsign,
                                        None,
                                        call.last_snr.round().clamp(-128.0, 127.0) as i8,
                                        0.0,
                                    );
                                    (score, call)
                                })
                                .collect();
                            scored.sort_by(|a, b| {
                                b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
                            });
                            recent_pool = scored.into_iter().map(|(_, call)| call).collect();
                        }
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

    /// PAN-27 finding 2 (round 4 review): a decoder rebuild (tier probe /
    /// live protocol/OSD/pass change, this file's decoder-thread loop
    /// around `Ft8Decoder::new(new_cfg)`) used to only reseed the new
    /// decoder with the station's own callsign, discarding every OTHER
    /// caller the round-2 standard-decode seeding loop had already
    /// learned. This reproduces the fix: carry
    /// `Ft8Decoder::learned_callsigns()` from the OLD decoder into the NEW
    /// one, the same way the rebuild path does.
    #[test]
    fn decoder_rebuild_carries_learned_callsigns_forward() {
        let cfg = Ft8Config::default();
        let mut old_decoder = Ft8Decoder::new(cfg.clone()).expect("decoder ctor");

        // The round-2 seeding loop learned K1DEF from an earlier standard
        // decode this session (decoder.rs's own tests already cover that
        // seeding path end-to-end via decode_window; seeded directly here
        // to isolate the rebuild-carryover behavior under test).
        old_decoder.seed_hash_callsign("K1DEF");

        // Rebuild: a fresh decoder built the way the tier-probe/protocol-
        // change path does, PLUS the fix -- carry the old table's learned
        // callsigns forward, mirroring the rebuild arm in this file.
        let mut new_decoder = Ft8Decoder::new(cfg).expect("decoder ctor");
        new_decoder.seed_hash_callsign("K5ARH"); // station's own callsign
        for call in old_decoder.learned_callsigns() {
            new_decoder.seed_hash_callsign(&call);
        }

        // A compound-call CQer's reply that hashes K1DEF into the 12-bit
        // slot must resolve on the NEW (rebuilt) decoder, not render
        // <...> as it did before the fix.
        let mut encoder = pancetta_ft8::Ft8Encoder::new();
        let symbols = encoder
            .encode_message("YS/WE9G K1DEF RR73", None)
            .expect("encode i3=4 reply");
        let mut modulator = pancetta_ft8::Ft8Modulator::new_default().expect("modulator");
        let mut tx = modulator
            .modulate_symbols(&symbols, 500.0)
            .expect("modulate");
        tx.resize(WINDOW_SAMPLES, 0.0);

        let decoded = new_decoder.decode_window(&tx).expect("decode i3=4 reply");
        let hit = decoded
            .iter()
            .find(|d| d.message.to_callsign.as_deref() == Some("YS/WE9G"))
            .expect("must decode the i3=4 reply addressed to YS/WE9G");
        assert_eq!(
            hit.message.from_callsign.as_deref(),
            Some("<K1DEF>"),
            "K1DEF must resolve on the REBUILT decoder via carried-over \
             learned callsigns, not render as <...>"
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

#[cfg(test)]
mod slot_parity_for_receipt_tests {
    use super::slot_parity_for_receipt;
    use chrono::{DateTime, Duration, Utc};
    use pancetta_core::slot::SlotParity;

    /// mode=FT8 regression: feeding the historical hardcoded values (13s
    /// decode phase, 15s slot) reproduces the same parity `SlotParity::of`
    /// (which hardcodes those exact constants) would give — the
    /// live-mode-switch fix must not change FT8 behavior.
    #[test]
    fn ft8_params_match_legacy_hardcoded_formula() {
        let received: DateTime<Utc> = DateTime::from_timestamp_nanos(20_000_000_000); // t=20s
        let ft8_decode_phase = Duration::nanoseconds(13_000_000_000); // 13s
        let ft8_slot_ns = 15_000_000_000i64; // 15s

        let via_helper = slot_parity_for_receipt(received, ft8_decode_phase, ft8_slot_ns);
        let via_legacy_formula = SlotParity::of(received - ft8_decode_phase);
        assert_eq!(via_helper, via_legacy_formula);
        // Concretely: slot_start = 20s - 13s = 7s, which falls in the [0,15)
        // slot -> index 0 -> Even.
        assert_eq!(via_helper, SlotParity::Even);
    }

    /// This is the regression test for the Critical whole-branch-review
    /// finding: the decode loop used to read `active_slot_ns`/
    /// `active_decode_phase_ns` ONCE at thread startup and never again, so a
    /// live mode switch (Shift+M) never changed the parity stamped on
    /// subsequently decoded frames. `slot_parity_for_receipt` is the pure
    /// function the (now-fixed) loop calls with freshly-read atomics every
    /// iteration; this test proves the SAME receipt instant produces a
    /// DIFFERENT parity when fed FT4's period instead of FT8's — i.e. the
    /// function has no memory of a prior call and always reflects whatever
    /// (slot_ns, decode_phase) it's given "this iteration". If the loop
    /// were still caching the pre-switch (FT8) values, every post-switch
    /// window would keep landing on the FT8 answer.
    #[test]
    fn same_receipt_instant_differs_by_active_mode_period() {
        let received: DateTime<Utc> = DateTime::from_timestamp_nanos(20_000_000_000); // t=20s

        let ft8_decode_phase = Duration::nanoseconds(13_000_000_000); // 13s
        let ft8_slot_ns = 15_000_000_000i64; // 15s
        let ft8_parity = slot_parity_for_receipt(received, ft8_decode_phase, ft8_slot_ns);

        let ft4_decode_phase = Duration::nanoseconds(6_500_000_000); // 6.5s
        let ft4_slot_ns = 7_500_000_000i64; // 7.5s
        let ft4_parity = slot_parity_for_receipt(received, ft4_decode_phase, ft4_slot_ns);

        // FT8: slot_start = 20 - 13 = 7s -> index 7/15 = 0 -> Even.
        assert_eq!(ft8_parity, SlotParity::Even);
        // FT4: slot_start = 20 - 6.5 = 13.5s -> index 13.5/7.5 = 1 -> Odd.
        assert_eq!(ft4_parity, SlotParity::Odd);
        // The critical property: same instant, different mode period ->
        // different (correct) parity. A stale-cache bug would keep
        // returning `ft8_parity` for both.
        assert_ne!(ft8_parity, ft4_parity);
    }
}

#[cfg(test)]
mod cross_time_decode_record_tests {
    use super::cross_time_decode_record;
    use pancetta_core::slot::SlotParity;
    use pancetta_ft8::{DecodedMessage, Ft8Message};

    /// PAN-146: the record carries the window's captured dial and slot grid.
    #[test]
    fn carries_captured_dial_and_slot_grid() {
        let mut d = DecodedMessage::new(Ft8Message::default(), -10.0, 0.5, 1500.0, 0.1);
        d.message.from_callsign = Some("5Z4VJ".to_string());
        d.slot_parity = Some(SlotParity::Odd);
        d.captured_dial_hz = Some(21_074_000);
        let rec = cross_time_decode_record(&d, 7_500_000_000);
        assert_eq!(rec.slot_parity, Some(1));
        assert_eq!(rec.captured_dial_hz, 21_074_000);
        assert_eq!(rec.slot_ns, 7_500_000_000);
    }

    /// A message with no captured dial records the unknown-0 sentinel.
    #[test]
    fn missing_captured_dial_records_unknown_sentinel() {
        let d = DecodedMessage::new(Ft8Message::default(), -10.0, 0.5, 1500.0, 0.1);
        assert_eq!(
            cross_time_decode_record(&d, 15_000_000_000).captured_dial_hz,
            0
        );
    }
}

/// PAN-165: the pure helpers behind the hot loop's `window timing` line.
#[cfg(test)]
mod window_timing_tests {
    use super::{headroom_ms, over_deadline_ms};
    use pancetta_ft8::DecodeBudget;
    use std::time::{Duration, Instant};

    #[test]
    fn headroom_is_deadline_minus_instant() {
        let base = Instant::now();
        let b = DecodeBudget::until(base + Duration::from_millis(2000));
        assert_eq!(
            headroom_ms(&b, base + Duration::from_millis(500)),
            Some(1500)
        );
    }

    #[test]
    fn headroom_saturates_at_zero_past_the_deadline() {
        let base = Instant::now();
        let b = DecodeBudget::until(base + Duration::from_millis(2000));
        assert_eq!(headroom_ms(&b, base + Duration::from_millis(2600)), Some(0));
    }

    #[test]
    fn over_deadline_is_zero_on_time_and_positive_when_late() {
        let base = Instant::now();
        let b = DecodeBudget::until(base + Duration::from_millis(2000));
        assert_eq!(over_deadline_ms(&b, base + Duration::from_millis(1999)), 0);
        assert_eq!(
            over_deadline_ms(&b, base + Duration::from_millis(2600)),
            600
        );
    }

    #[test]
    fn unlimited_budget_has_no_headroom_figure_and_never_overruns() {
        let b = DecodeBudget::unlimited();
        assert_eq!(headroom_ms(&b, Instant::now()), None);
        assert_eq!(over_deadline_ms(&b, Instant::now()), 0);
    }
}

#[cfg(test)]
mod decode_budget_ceiling_ms_tests {
    use super::decode_budget_ceiling_ms;

    #[test]
    fn ft8_slot_period_gets_2000ms_ceiling() {
        assert_eq!(decode_budget_ceiling_ms(15_000_000_000), 2000);
    }

    #[test]
    fn ft4_slot_period_gets_800ms_ceiling() {
        assert_eq!(decode_budget_ceiling_ms(7_500_000_000), 800);
    }

    #[test]
    fn boundary_at_7_5s_is_inclusive_on_the_ft4_side() {
        // Exactly FT4's period: must resolve to the tighter (800ms) ceiling.
        assert_eq!(decode_budget_ceiling_ms(7_500_000_000), 800);
        // One ns above the boundary: must resolve to the FT8 (2000ms) ceiling.
        assert_eq!(decode_budget_ceiling_ms(7_500_000_001), 2000);
        // One ns below the boundary: stays on the FT4 (800ms) side.
        assert_eq!(decode_budget_ceiling_ms(7_499_999_999), 800);
    }

    #[test]
    fn zero_slot_ns_is_treated_as_the_tighter_ceiling() {
        // Degenerate input; falls on the <= branch, so 800ms — never panics.
        assert_eq!(decode_budget_ceiling_ms(0), 800);
    }
}

/// PAN-164: the hot loop takes the budget and the decoder config from ONE
/// `DecodeWindowConfig` publish per window, never one publish's budget with
/// another's config. The old shape (a budget atomic loaded, then a separate
/// config `try_read`) tore whenever a writer's whole critical section
/// landed between the two reads; these tests pin the one-read contract.
#[cfg(test)]
mod decode_window_refresh_tests {
    use super::super::effort::{apply_effort_overrides, DecodeWindowConfig};
    use super::{
        refresh_decode_window, AppliedDecodeWindow, DecodeWindowRefresh, DecoderRebuildKey,
    };
    use pancetta_config::DecodeEffort;
    use pancetta_ft8::{tier_probe::HardwareTier, Ft8Config, Protocol};
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, Barrier};

    /// Standard on FT8: `(250 ms, 1 pass)`.
    fn standard() -> DecodeWindowConfig {
        let mut w = DecodeWindowConfig::new(Ft8Config::default(), 0);
        w.publish_effort(DecodeEffort::Standard, HardwareTier::Fast, 250);
        w
    }

    fn pair(a: &AppliedDecodeWindow) -> (u64, usize) {
        (a.budget_ms, a.rebuild_key.max_decode_passes)
    }

    const OLD: (u64, usize) = (250, 1);
    const NEW: (u64, usize) = (0, 2);

    fn assert_untorn(a: &AppliedDecodeWindow) {
        let p = pair(a);
        assert!(
            p == OLD || p == NEW,
            "torn decode window: (budget_ms, max_decode_passes) = {p:?}, \
             expected {OLD:?} or {NEW:?}"
        );
    }

    #[test]
    fn refresh_adopts_budget_and_config_from_one_publish() {
        let shared = tokio::sync::RwLock::new(standard());
        let mut applied = AppliedDecodeWindow::of(&standard());
        assert_eq!(pair(&applied), OLD);

        shared.try_write().expect("uncontended").publish_effort(
            DecodeEffort::Max,
            HardwareTier::Fast,
            0,
        );

        let refresh = refresh_decode_window(&shared, &mut applied, |cfg| {
            assert_eq!(cfg.max_decode_passes, 2);
            Ok::<_, ()>(())
        });
        assert_eq!(refresh, DecodeWindowRefresh::Rebuilt);
        assert_eq!(pair(&applied), NEW);
        assert_eq!(applied.effective_budget_ms(), 2000);
    }

    #[test]
    fn refresh_while_a_writer_holds_the_lock_keeps_the_whole_previous_window() {
        let shared = tokio::sync::RwLock::new(standard());
        let mut applied = AppliedDecodeWindow::of(&standard());

        let mut g = shared.try_write().expect("uncontended");
        g.publish_effort(DecodeEffort::Max, HardwareTier::Fast, 0);
        let refresh = refresh_decode_window(&shared, &mut applied, |_| -> Result<(), ()> {
            panic!("no rebuild may run while a writer holds the window")
        });
        assert_eq!(refresh, DecodeWindowRefresh::Busy);
        assert_eq!(
            pair(&applied),
            OLD,
            "a busy window keeps the whole old pair"
        );

        drop(g);
        let refresh = refresh_decode_window(&shared, &mut applied, |_| Ok::<_, ()>(()));
        assert_eq!(refresh, DecodeWindowRefresh::Rebuilt);
        assert_eq!(pair(&applied), NEW);
    }

    #[test]
    fn budget_only_publish_is_adopted_without_a_rebuild() {
        let shared = tokio::sync::RwLock::new(standard());
        let mut applied = AppliedDecodeWindow::of(&standard());

        // Standard -> Deep: 250 -> 1000 ms, same decoder fields.
        shared.try_write().expect("uncontended").publish_effort(
            DecodeEffort::Deep,
            HardwareTier::Fast,
            1000,
        );
        let refresh = refresh_decode_window(&shared, &mut applied, |_| -> Result<(), ()> {
            panic!("a budget-only publish must not rebuild the decoder")
        });
        assert_eq!(refresh, DecodeWindowRefresh::Adopted);
        assert_eq!(applied.budget_ms, 1000);
        assert_eq!(applied.rebuild_key.max_decode_passes, 1);
    }

    #[test]
    fn failed_rebuild_keeps_the_previous_decoder_and_its_budget() {
        let shared = tokio::sync::RwLock::new(standard());
        let mut applied = AppliedDecodeWindow::of(&standard());

        shared.try_write().expect("uncontended").publish_effort(
            DecodeEffort::Max,
            HardwareTier::Fast,
            0,
        );
        let refresh = refresh_decode_window(&shared, &mut applied, |_| Err("boom"));
        assert_eq!(refresh, DecodeWindowRefresh::RebuildFailed("boom"));
        assert_eq!(
            pair(&applied),
            OLD,
            "the new budget must not be adopted without the decoder it was published with"
        );

        let refresh = refresh_decode_window(&shared, &mut applied, |_| Ok::<_, &str>(()));
        assert_eq!(refresh, DecodeWindowRefresh::Rebuilt);
        assert_eq!(pair(&applied), NEW);
    }

    /// The ticket's reproduction shape, against the new code: the writer's
    /// whole critical section runs between two windows, then the writer
    /// holds the guard across a window. Every window sees a whole publish.
    #[test]
    fn pan164_writer_critical_section_between_two_windows_cannot_tear() {
        let shared = Arc::new(tokio::sync::RwLock::new(standard()));
        let barrier = Arc::new(Barrier::new(2));

        let writer = {
            let shared = Arc::clone(&shared);
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                // Round 1: a whole publish between window N and N+1.
                barrier.wait(); // A: reader finished window N
                {
                    let mut g = shared.blocking_write();
                    g.publish_effort(DecodeEffort::Max, HardwareTier::Fast, 0);
                }
                barrier.wait(); // B: publish done
                barrier.wait(); // B2: reader finished window N+1

                // Round 2: hold the guard across the reader's window.
                let mut g = shared.blocking_write();
                barrier.wait(); // C: guard held
                barrier.wait(); // D: reader's window ran
                g.publish_effort(DecodeEffort::Standard, HardwareTier::Fast, 250);
                drop(g);
                barrier.wait(); // E: publish done
            })
        };

        let mut applied = AppliedDecodeWindow::of(&standard());
        let mut decoder_passes = applied.rebuild_key.max_decode_passes;
        let mut window = |applied: &mut AppliedDecodeWindow| {
            let r = refresh_decode_window(&shared, applied, |cfg| {
                decoder_passes = cfg.max_decode_passes;
                Ok::<_, ()>(())
            });
            assert_untorn(applied);
            r
        };

        assert_eq!(window(&mut applied), DecodeWindowRefresh::Adopted);
        assert_eq!(pair(&applied), OLD);
        barrier.wait(); // A
        barrier.wait(); // B
        assert_eq!(window(&mut applied), DecodeWindowRefresh::Rebuilt);
        assert_eq!(pair(&applied), NEW);
        barrier.wait(); // B2

        barrier.wait(); // C
        assert_eq!(window(&mut applied), DecodeWindowRefresh::Busy);
        assert_eq!(
            pair(&applied),
            NEW,
            "a busy window keeps the whole NEW pair"
        );
        barrier.wait(); // D
        barrier.wait(); // E
        assert_eq!(window(&mut applied), DecodeWindowRefresh::Rebuilt);
        assert_eq!(pair(&applied), OLD);
        assert_eq!(decoder_passes, 1);

        writer.join().expect("writer thread");
    }

    /// Free-running stress: a writer flips between two consistent
    /// publishes as fast as it can while the reader refreshes. On the old
    /// two-read shape the equivalent loop tore 21–73 552 times per 200k
    /// reads (plan, Current State §1); with one read it cannot tear.
    #[test]
    fn concurrent_publishes_never_yield_a_torn_window() {
        let shared = Arc::new(tokio::sync::RwLock::new(standard()));
        let stop = Arc::new(AtomicBool::new(false));
        let publishes = Arc::new(AtomicU64::new(0));

        let writer = {
            let shared = Arc::clone(&shared);
            let stop = Arc::clone(&stop);
            let publishes = Arc::clone(&publishes);
            std::thread::spawn(move || {
                let mut max = true;
                while !stop.load(Ordering::Relaxed) {
                    let mut g = shared.blocking_write();
                    if max {
                        g.publish_effort(DecodeEffort::Max, HardwareTier::Fast, 0);
                    } else {
                        g.publish_effort(DecodeEffort::Standard, HardwareTier::Fast, 250);
                    }
                    drop(g);
                    max = !max;
                    publishes.fetch_add(1, Ordering::Relaxed);
                }
            })
        };

        let mut applied = AppliedDecodeWindow::of(&standard());
        let mut decoder_passes = applied.rebuild_key.max_decode_passes;
        let mut refreshes: u64 = 0;
        while refreshes < 50_000 || publishes.load(Ordering::Relaxed) < 1_000 {
            let _ = refresh_decode_window(&shared, &mut applied, |cfg| {
                decoder_passes = cfg.max_decode_passes;
                Ok::<_, ()>(())
            });
            refreshes += 1;
            assert_untorn(&applied);
            assert_eq!(
                applied.rebuild_key.max_decode_passes, decoder_passes,
                "the adopted window must describe the decoder actually built"
            );
        }
        stop.store(true, Ordering::Relaxed);
        writer.join().expect("writer thread");
    }

    #[test]
    fn effective_budget_uses_the_decoders_own_protocol_ceiling() {
        let cases = [
            (Protocol::Ft8, 0, 2000),
            (Protocol::Ft4, 0, 800),
            (Protocol::Ft8, 250, 250),
            (Protocol::Ft8, 5000, 2000),
            (Protocol::Ft4, 1000, 800),
        ];
        for (protocol, budget_ms, expected) in cases {
            let cfg = Ft8Config {
                protocol,
                ..Ft8Config::default()
            };
            let applied = AppliedDecodeWindow::of(&DecodeWindowConfig::new(cfg, budget_ms));
            assert_eq!(
                applied.effective_budget_ms(),
                expected,
                "{protocol:?} with budget {budget_ms} ms"
            );
        }
    }

    // --------------------------------------------------------------
    // PAN-158: entering or leaving FT8 Eco's fine sync rebuilds
    // --------------------------------------------------------------

    /// Eco on FT8: `(1 ms, fine sync on)`.
    fn eco() -> DecodeWindowConfig {
        let mut w = DecodeWindowConfig::new(Ft8Config::default(), 0);
        w.publish_effort(DecodeEffort::Eco, HardwareTier::Fast, 1);
        w
    }

    fn fine_sync_pair(a: &AppliedDecodeWindow) -> (u64, bool) {
        (a.budget_ms, a.rebuild_key.fine_sync_enabled)
    }

    #[test]
    fn fine_sync_flag_alone_changes_the_rebuild_key() {
        let off = Ft8Config::default();
        let on = Ft8Config {
            fine_sync_enabled: true,
            ..Ft8Config::default()
        };
        assert_ne!(DecoderRebuildKey::of(&off), DecoderRebuildKey::of(&on));
    }

    /// `every_effort_override_field_is_a_rebuild_trigger` cannot catch a
    /// missing knee field: the knee only ever changes together with
    /// `protocol`, which is already in the key.
    #[test]
    fn impulse_robust_llr_alone_changes_the_rebuild_key() {
        let off = Ft8Config::default();
        let on = Ft8Config {
            impulse_robust_llr: Some(6.0),
            ..Ft8Config::default()
        };
        let other = Ft8Config {
            impulse_robust_llr: Some(3.0),
            ..Ft8Config::default()
        };
        assert_ne!(DecoderRebuildKey::of(&off), DecoderRebuildKey::of(&on));
        assert_ne!(DecoderRebuildKey::of(&on), DecoderRebuildKey::of(&other));
    }

    #[test]
    fn entering_and_leaving_eco_rebuilds_with_the_published_flag() {
        let shared = tokio::sync::RwLock::new(standard());
        let mut applied = AppliedDecodeWindow::of(&standard());
        assert_eq!(fine_sync_pair(&applied), (250, false));

        shared.try_write().expect("uncontended").publish_effort(
            DecodeEffort::Eco,
            HardwareTier::Fast,
            1,
        );
        let mut built_with = None;
        let refresh = refresh_decode_window(&shared, &mut applied, |cfg| {
            built_with = Some(cfg.fine_sync_enabled);
            Ok::<_, ()>(())
        });
        assert_eq!(refresh, DecodeWindowRefresh::Rebuilt);
        assert_eq!(built_with, Some(true));
        assert_eq!(fine_sync_pair(&applied), (1, true));

        shared.try_write().expect("uncontended").publish_effort(
            DecodeEffort::Standard,
            HardwareTier::Fast,
            250,
        );
        let mut built_with = None;
        let refresh = refresh_decode_window(&shared, &mut applied, |cfg| {
            built_with = Some(cfg.fine_sync_enabled);
            Ok::<_, ()>(())
        });
        assert_eq!(refresh, DecodeWindowRefresh::Rebuilt);
        assert_eq!(built_with, Some(false));
        assert_eq!(fine_sync_pair(&applied), (250, false));
    }

    /// Standard with a 1 ms override and Eco publish the same budget, so
    /// only the flag can trigger this rebuild.
    #[test]
    fn same_budget_standard_to_eco_rebuilds_on_the_flag_alone() {
        let mut standard_1ms = DecodeWindowConfig::new(Ft8Config::default(), 0);
        standard_1ms.publish_effort(DecodeEffort::Standard, HardwareTier::Fast, 1);
        let shared = tokio::sync::RwLock::new(standard_1ms.clone());
        let mut applied = AppliedDecodeWindow::of(&standard_1ms);
        assert_eq!(fine_sync_pair(&applied), (1, false));

        shared.try_write().expect("uncontended").publish_effort(
            DecodeEffort::Eco,
            HardwareTier::Fast,
            1,
        );
        let mut built_with = None;
        let refresh = refresh_decode_window(&shared, &mut applied, |cfg| {
            built_with = Some(cfg.fine_sync_enabled);
            Ok::<_, ()>(())
        });
        assert_eq!(refresh, DecodeWindowRefresh::Rebuilt);
        assert_eq!(built_with, Some(true));
        assert_eq!(fine_sync_pair(&applied), (1, true));
    }

    #[test]
    fn repeating_an_eligible_eco_publish_does_not_rebuild() {
        let shared = tokio::sync::RwLock::new(eco());
        let mut applied = AppliedDecodeWindow::of(&eco());

        shared.try_write().expect("uncontended").publish_effort(
            DecodeEffort::Eco,
            HardwareTier::Fast,
            1,
        );
        let refresh = refresh_decode_window(&shared, &mut applied, |_| -> Result<(), ()> {
            panic!("an identical publish must not rebuild the decoder")
        });
        assert_eq!(refresh, DecodeWindowRefresh::Adopted);
        assert_eq!(fine_sync_pair(&applied), (1, true));
    }

    #[test]
    fn busy_or_failed_eco_transition_keeps_the_whole_previous_window() {
        let shared = tokio::sync::RwLock::new(standard());
        let mut applied = AppliedDecodeWindow::of(&standard());

        let mut g = shared.try_write().expect("uncontended");
        g.publish_effort(DecodeEffort::Eco, HardwareTier::Fast, 1);
        let refresh = refresh_decode_window(&shared, &mut applied, |_| -> Result<(), ()> {
            panic!("no rebuild may run while a writer holds the window")
        });
        assert_eq!(refresh, DecodeWindowRefresh::Busy);
        assert_eq!(fine_sync_pair(&applied), (250, false));
        drop(g);

        let refresh = refresh_decode_window(&shared, &mut applied, |_| Err("boom"));
        assert_eq!(refresh, DecodeWindowRefresh::RebuildFailed("boom"));
        assert_eq!(
            fine_sync_pair(&applied),
            (250, false),
            "Eco's 1 ms budget must not be adopted without its fine-sync decoder"
        );

        let mut built_with = None;
        let refresh = refresh_decode_window(&shared, &mut applied, |cfg| {
            built_with = Some(cfg.fine_sync_enabled);
            Ok::<_, &str>(())
        });
        assert_eq!(refresh, DecodeWindowRefresh::Rebuilt);
        assert_eq!(built_with, Some(true));
        assert_eq!(fine_sync_pair(&applied), (1, true));
    }

    /// Generalisation guard: any `Ft8Config` field `apply_effort_overrides`
    /// writes must be a decoder-rebuild trigger, or the hot loop would
    /// adopt a publish's budget while decoding with the old value of that
    /// field.
    #[test]
    fn every_effort_override_field_is_a_rebuild_trigger() {
        let mut configs: Vec<(String, String, DecoderRebuildKey)> = Vec::new();
        for effort in [
            DecodeEffort::Eco,
            DecodeEffort::Standard,
            DecodeEffort::Deep,
            DecodeEffort::Max,
            DecodeEffort::Auto,
        ] {
            for tier in [
                HardwareTier::Slow,
                HardwareTier::Moderate,
                HardwareTier::Fast,
            ] {
                for budget_ms in [0u64, 1, 250, 1000, 2000] {
                    for protocol in [Protocol::Ft8, Protocol::Ft4] {
                        let mut cfg = Ft8Config {
                            protocol,
                            ..Ft8Config::default()
                        };
                        apply_effort_overrides(effort, tier, budget_ms, &mut cfg);
                        configs.push((
                            format!("{effort:?}/{tier:?}/{budget_ms}ms/{protocol:?}"),
                            format!("{cfg:?}"),
                            DecoderRebuildKey::of(&cfg),
                        ));
                    }
                }
            }
        }
        for (label_a, debug_a, key_a) in &configs {
            for (label_b, debug_b, key_b) in &configs {
                if debug_a != debug_b {
                    assert_ne!(
                        key_a, key_b,
                        "{label_a} vs {label_b}: a field apply_effort_overrides writes \
                         is missing from DecoderRebuildKey"
                    );
                }
            }
        }
    }
}
