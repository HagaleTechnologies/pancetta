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
/// Scope: this ceiling (and `decode_effort_budget_ms`) only governs the
/// native `pancetta-ft8` AP-enhanced decode call. The separate
/// `decode_window_ft8lib_protocol` ft8_lib C FFI decode path (below) runs
/// unconditionally and is not bounded by either budget.
fn decode_budget_ceiling_ms(slot_ns: u64) -> u64 {
    if slot_ns <= 7_500_000_000 {
        800
    } else {
        2000
    }
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

/// PAN-40 round-2 review (Codex finding 2): the unresolved placeholder
/// `"<...>"` has erased the underlying 12-bit hash entirely, so on a busy
/// band two DIFFERENT stations replying to the same third party in the
/// same slot (e.g. simultaneous `<...> YS/WE9G RR73` and `<K1ABC>
/// YS/WE9G RR73` from two distinct callers) can look like a hash twin
/// under frequency-proximity-plus-normalized-text alone. Neither decoder
/// backend exposes the raw hash value or a payload-level fingerprint at
/// this layer (ft8lib is a black-box FFI call returning only rendered
/// text + freq/SNR/DT; see `DecodedMessage::from_ft8lib`), so real
/// hash/payload identity isn't available here — per PAN-40's own bias
/// ("dropping a real decode can silently kill a live QSO"), the fix is
/// to tighten proximity requirements substantially rather than keep the
/// old, much looser gate.
///
/// PAN-40 round-3 review (Codex finding 3): a fixed FT8-tuned Hz
/// tolerance is wrong for FT4. This coordinator path decodes whatever
/// `[decoder].protocol` is currently active (see `last_protocol` at the
/// call site) — FT8's sub-bin spacing is `tone_spacing(6.25) /
/// FREQ_OSR(2) = 3.125 Hz`; FT4's is `tone_spacing(20.8333) / FREQ_OSR(2)
/// ≈ 10.42 Hz` (`pancetta_ft8::ProtocolParams::ft4`). `FREQ_OSR` itself
/// is NOT protocol-dependent — it's a fixed `2` in both the native
/// decoder (`decoder.rs::FREQ_OSR`) and the ft8lib FFI's
/// `monitor_config_t` (`ft8_lib_ffi.rs`) — so deriving from
/// `tone_spacing` alone (without threading `FREQ_OSR` across the crate
/// boundary) exactly reproduces both backends' real sub-bin grid for
/// whichever protocol is active.
///
/// Max `frequency_offset` difference (Hz) to consider two decodes the
/// same physical signal: one sub-bin width for the active protocol, just
/// enough to absorb independent sub-bin peak-fit rounding between the
/// two backends while being far tighter than realistic distinct-station
/// spacing on a busy band.
fn hash_twin_freq_tolerance_hz(protocol: pancetta_ft8::Protocol) -> f64 {
    const FREQ_OSR: f64 = 2.0;
    pancetta_ft8::ProtocolParams::from_protocol(protocol).tone_spacing / FREQ_OSR
}

/// Max `time_offset` (DT, seconds) difference to consider two decodes
/// (after [`normalized_time_offset`] puts them on the same convention)
/// the same physical signal. Two backends independently sync-detecting
/// the SAME burst should agree on its arrival time to a small fraction
/// of a symbol period; two different transmitting stations essentially
/// never coincidentally share both near-identical frequency AND
/// near-identical DT AND matching non-hash text. `0.625` of a symbol
/// period reproduces the original FT8-only tolerance exactly (`0.16 s *
/// 0.625 = 0.1 s`) while scaling correctly for FT4's much shorter
/// (0.048 s) symbol period.
fn hash_twin_time_tolerance_s(protocol: pancetta_ft8::Protocol) -> f64 {
    pancetta_ft8::ProtocolParams::from_protocol(protocol).symbol_period * 0.625
}

/// PAN-40 round-3 review (Codex finding 1): does `msg` look like it came
/// from the ft8lib FFI backend (`DecodedMessage::from_ft8lib`) rather
/// than the native pipeline? `confidence_features` is only ever
/// populated by the native BP/OSD pipeline's `stamp_decode_origin` —
/// per that field's own doc comment, `None` means "predates stamping or
/// came from a path that doesn't stamp (FFI, tests constructing messages
/// directly)". ft8lib's FFI wrapper never touches this field (it stays
/// at the `Ft8Message::default()`-adjacent `None` `from_ft8lib` sets), so
/// this is a reliable, already-available discriminator — no new
/// plumbing needed.
fn is_ft8lib_origin_decode(msg: &pancetta_ft8::DecodedMessage) -> bool {
    msg.confidence_features.is_none()
}

/// PAN-40 round-3 review (Codex finding 1) — the actual bug: the DT
/// proximity gate compared `time_offset` values that use DIFFERENT
/// reporting conventions between the two backends for the SAME physical
/// candidate, so it could never pass for the real ft8lib/native twin
/// this whole PR exists to fix.
///
/// `ft8lib_decode_audio_protocol` (`pancetta-ft8/src/ft8_lib_ffi.rs`)
/// reports the raw upstream `time_sec = (time_offset + time_sub/osr) *
/// symbol_period` — ft8_lib's own convention, with no look-back
/// correction. The native decoder's `candidate_offset_samples`
/// (`pancetta-ft8/src/decoder.rs`) subtracts `SLIDING_FRAME_LOOKBACK_
/// STEPS` (2 time-steps at `TIME_OSR=2`, i.e. exactly **one symbol
/// period**) before converting to seconds — documented there as fixing
/// ft8lib's convention being measured "one symbol period late" against
/// synthetic ground truth. So for the SAME physical signal, ft8lib's
/// raw DT is systematically `+1 symbol period` relative to the native
/// decoder's corrected DT — 0.16 s for FT8, 0.048 s for FT4 — which
/// exceeds even the OLD, un-protocol-scaled 0.1 s tolerance, so the DT
/// gate silently rejected every real cross-backend twin.
///
/// Normalizes an ft8lib-origin decode's `time_offset` onto the native
/// decoder's (corrected) convention by subtracting one symbol period;
/// leaves a native-origin decode's `time_offset` untouched. Comparing
/// two NORMALIZED values is then apples-to-apples regardless of which
/// backend(s) produced them.
fn normalized_time_offset(msg: &pancetta_ft8::DecodedMessage, symbol_period_s: f64) -> f64 {
    if is_ft8lib_origin_decode(msg) {
        msg.time_offset - symbol_period_s
    } else {
        msg.time_offset
    }
}

/// PAN-40 round-4 review — general provenance check: does `msg` carry a
/// genuine bit-level LDPC/CRC-verified codeword, as opposed to being
/// built purely from a template/hypothesis's TEXT?
///
/// The native pipeline's real per-candidate decode path always
/// round-trips the actual demodulated codeword through
/// `MessageParser::parse_payload` (13 call sites across
/// `pancetta-ft8/src/decoder.rs`, covering every real decode pass:
/// standard passes 0/1, cross-cycle averaging, coherent multipass,
/// joint-pair retry, sync-relaxation, BICM-ID rescue, and the standard
/// fourth-pass-after-a7 iteration) — `parse_payload` always sets
/// `Ft8Message::payload_bits` to the real 77-bit codeword bits.
///
/// Every currently-known speculative/template-derived construction path
/// — the within-window a7 cross-correlation pass
/// (`a7_cross_correlation_pass`) AND the cross-sequence a7 consumer
/// (`try_cross_sequence_decodes`) — instead builds its `Ft8Message` via
/// `Ft8Message::from_text(template_text)`: parsing the WINNING
/// TEMPLATE's text, not a real decode. `from_text` never touches
/// `payload_bits`, leaving it at `Ft8Message::default()`'s empty
/// `BitVec`. (Confirmed exhaustively: `Ft8Message::from_text` has
/// exactly two call sites in `decoder.rs`, and both are these two a7
/// paths.)
///
/// This makes "does this message carry a non-empty `payload_bits`" a
/// single, forward-compatible provenance signal — round-2 excluded
/// cross-sequence a7 by name (`via_cross_sequence_a7`), round-3 then had
/// to add a second special case for within-window a7
/// (`decode_origin == 5`, ALSO used by the unrelated, genuinely-real
/// fourth-pass-after-a7). Both of those are one-mechanism-at-a-time
/// patches; this check instead asks "was a real decode ever performed",
/// so a THIRD speculative mechanism (now or added later) that similarly
/// builds from template text rather than decoded bits is excluded
/// automatically, without this function or its caller needing to know
/// it by name.
///
/// Must only be applied to native-pipeline messages — see
/// [`is_ineligible_for_hash_twin_removal`], which combines this with
/// [`is_ft8lib_origin_decode`] correctly. (ft8lib-FFI-origin decodes
/// ALSO have empty `payload_bits` — `from_ft8lib` re-parses the C
/// library's already-CRC-verified rendered TEXT via the same
/// `Ft8Message::from_text`, for the same reason a7's template text is
/// re-parsed — despite being genuine decodes: ft8_lib only ever returns
/// text for candidates that passed its own C-side CRC check. Applying
/// this check to an ft8lib-origin message would wrongly exclude it.)
fn native_decode_lacks_verified_payload(msg: &pancetta_ft8::DecodedMessage) -> bool {
    msg.message.payload_bits.is_empty()
}

/// PAN-40 round-4 review — the single, unified "is `msg` ineligible to
/// participate in hash-twin removal, on either side of the pair" check.
/// A message is ineligible iff it is a template/hypothesis-derived
/// candidate rather than a genuine decode of received audio: either the
/// legacy explicit cross-sequence-a7 marker is set, or (for a
/// native-pipeline message specifically — never an ft8lib-origin one,
/// see [`native_decode_lacks_verified_payload`]'s doc comment)
/// `payload_bits` shows no real LDPC/CRC decode ever happened.
///
/// `via_cross_sequence_a7` is redundant with the payload check today
/// (cross-sequence a7 also builds via `Ft8Message::from_text`, so it
/// also always has empty `payload_bits`) — kept anyway as a cheap,
/// intent-revealing, belt-and-suspenders check that stays correct even
/// if the cross-sequence path's construction ever changes.
fn is_ineligible_for_hash_twin_removal(msg: &pancetta_ft8::DecodedMessage) -> bool {
    msg.via_cross_sequence_a7
        || (!is_ft8lib_origin_decode(msg) && native_decode_lacks_verified_payload(msg))
}

/// PAN-40: is `word` the literal unresolved i3=4 hash-miss placeholder
/// token? Mirrors the `None`-branch semantics of
/// `pancetta_core::callsign::resolve_hash_render` (private to that
/// crate) applied to a single whitespace-split token rather than a full
/// callsign string.
fn is_unresolved_hash_token(word: &str) -> bool {
    word == "<...>"
}

/// PAN-40: normalize a single decode-text token for hash-twin
/// comparison. A resolved i3=4 hash render (`"<K5ARH>"`) unwraps to its
/// plain callsign (`"K5ARH"`); every other token (including the
/// unresolved placeholder, handled separately by
/// [`is_unresolved_hash_token`]) passes through unchanged.
fn normalize_hash_bracket_word(word: &str) -> &str {
    if word.len() >= 2
        && word.starts_with('<')
        && word.ends_with('>')
        && !is_unresolved_hash_token(word)
    {
        &word[1..word.len() - 1]
    } else {
        word
    }
}

/// PAN-40 — root cause: `callsigns_match` (pancetta-core) correctly
/// refuses, by design, to ever match the unresolved i3=4 hash-miss
/// placeholder `"<...>"` against a QSO's known partner — it carries no
/// identity information at all. But the ft8lib+native decoder merge
/// (the loop just above this function's call site) deduped only by
/// *exact* decode text: when the two backends decode the same physical
/// signal differently — one resolving the hash render, one leaving it
/// `"<...>"` — the texts differ, so both survive the merge and both get
/// forwarded to the QSO engine. The unresolved twin is then correctly
/// (but uselessly) rejected by `callsigns_match` every single cycle,
/// which can burn an entire watchdog window on a QSO whose partner was
/// actually replying the whole time (see PR body for the live-log
/// evidence: QSO `d8bc41ca-0da8-4e6b-bef9-23ae05ccbeb2` vs 3B9/SQ9UM).
///
/// Declares two decodes twins of the same signal only when ALL hold:
///   - their `frequency_offset`s agree within
///     [`hash_twin_freq_tolerance_hz`] for the active protocol (checked
///     by the caller, [`drop_unresolved_hash_twins`], since that's where
///     the full `DecodedMessage` — not just text — is available), AND
///   - their [`normalized_time_offset`] (DT, put on a common convention
///     across backends) agree within [`hash_twin_time_tolerance_s`] for
///     the active protocol (also checked by the caller), AND
///   - neither side is a speculative, template/hypothesis-derived
///     candidate (checked by the caller via
///     [`is_ineligible_for_hash_twin_removal`] — see its doc comment,
///     PAN-40 round-4 review), AND
///   - after normalizing away hash-resolution differences (unwrapping
///     resolved `"<CALL>"` renders to plain `"CALL"`, and treating the
///     literal unresolved placeholder `"<...>"` as a wildcard that
///     matches any single token at that position), the texts are
///     token-for-token identical.
///
/// This function itself only judges the text dimension; see
/// [`drop_unresolved_hash_twins`] for the freq+DT gating that must also
/// pass before a pair is treated as twins.
///
/// When a pair is declared twins, only the more-resolved side (the one
/// without the placeholder) is kept. Deliberately conservative: any
/// non-wildcard token mismatch aborts twin detection for that pair, and
/// two decodes where NEITHER side has the placeholder are never twins
/// here (if two backends fully and identically resolved the same
/// signal, their texts are literally equal and were already deduped
/// before this pass ever sees them). Per PAN-40's stated bias, an
/// ambiguous pair keeps BOTH rather than risk dropping a genuine decode:
/// a spurious extra decode reaching the QSO engine is harmless (it's
/// just correctly rejected if irrelevant), while dropping a real decode
/// can silently kill a live QSO.
///
/// Returns `Some(true)` if `a` is the side to keep, `Some(false)` if `b`
/// is, `None` if the pair are not (confidently) twins.
fn hash_twin_keep_first(a: &str, b: &str) -> Option<bool> {
    let ta: Vec<&str> = a.split_whitespace().collect();
    let tb: Vec<&str> = b.split_whitespace().collect();
    if ta.is_empty() || ta.len() != tb.len() {
        return None;
    }

    let mut a_has_placeholder = false;
    let mut b_has_placeholder = false;
    let mut saw_wildcard_position = false;

    for (wa, wb) in ta.iter().zip(tb.iter()) {
        let a_unresolved = is_unresolved_hash_token(wa);
        let b_unresolved = is_unresolved_hash_token(wb);
        a_has_placeholder |= a_unresolved;
        b_has_placeholder |= b_unresolved;

        if a_unresolved || b_unresolved {
            // A placeholder position matches anything on the other side
            // (it carries no identity information to contradict). If
            // BOTH sides are the placeholder at this position that's
            // also a compatible (equal) position.
            if !(a_unresolved && b_unresolved) {
                saw_wildcard_position = true;
            }
            continue;
        }

        if normalize_hash_bracket_word(wa) != normalize_hash_bracket_word(wb) {
            // Genuine content mismatch at a non-placeholder position —
            // these are two different real decodes, not a hash twin.
            return None;
        }
    }

    if !saw_wildcard_position {
        // The texts differ (callers only consider non-identical pairs)
        // but not because of a placeholder difference — not our case.
        return None;
    }

    match (a_has_placeholder, b_has_placeholder) {
        (true, false) => Some(false), // b is the resolved side
        (false, true) => Some(true),  // a is the resolved side
        // Both sides carry the placeholder somewhere, or neither does —
        // genuinely ambiguous which (if either) is "more resolved".
        // Keep both per the conservative bias above.
        _ => None,
    }
}

/// PAN-40 — drop the unresolved-hash-placeholder half of any decode pair
/// in `messages` that is confidently the same physical signal decoded
/// twice (once resolved, once not) by the two decoder backends: their
/// `frequency_offset`s agree within [`hash_twin_freq_tolerance_hz`],
/// their [`normalized_time_offset`]s agree within
/// [`hash_twin_time_tolerance_s`], neither side is a speculative
/// template/hypothesis-derived candidate (per
/// [`is_ineligible_for_hash_twin_removal`]), and
/// [`hash_twin_keep_first`] confirms the texts are twins modulo hash
/// resolution. See those functions' doc comments for the full rationale
/// and the conservative-by-default policy — a pair that fails ANY check
/// keeps both messages.
///
/// `protocol` is the coordinator's currently-active decode protocol
/// (`last_protocol` at the call site) — PAN-40 round-3 review finding 3:
/// this coordinator path also decodes FT4, whose tone spacing and symbol
/// period differ substantially from FT8's, so the freq/DT tolerances
/// MUST be derived per-call from the active protocol rather than
/// hardcoded to FT8's numbers (see [`hash_twin_freq_tolerance_hz`] /
/// [`hash_twin_time_tolerance_s`]).
///
/// PAN-40 round-2 review (Codex finding 1): callers MUST run this pass
/// AFTER `partition_ap_eval_decodes`, never before. `[decoder].
/// ap_eval_mode`'s guarantee is that every `ap_level == 0` decode reaches
/// every consumer unaffected. If this pass ran first and paired a
/// non-AP (`ap_level == 0`) placeholder decode with an AP-derived
/// (`ap_level > 0`) resolved twin, it would keep the AP copy and drop
/// the non-AP copy — `partition_ap_eval_decodes` then suppresses that
/// surviving AP copy too (it's `ap_level > 0`), so BOTH decodes vanish,
/// breaking the eval-mode guarantee for exactly the case eval mode
/// exists to observe (a resolved AP candidate that might be a phantom).
/// Running this pass on the post-partition "delivered" set instead means
/// it never sees (and so can never be tricked by) a decode AP-eval-mode
/// has already decided to suppress.
///
/// PAN-40 round-3 review (Codex finding 2) then round-4 review
/// (generalized): a pair is never eligible for twin-removal if EITHER
/// side is a speculative, template/hypothesis-derived candidate rather
/// than a genuine decode of received audio — see
/// [`is_ineligible_for_hash_twin_removal`]'s doc comment for the general
/// mechanism. If a speculative candidate were allowed to "win" a twin
/// comparison it could cause a real, CRC-valid decode (e.g. the genuine
/// unresolved-hash frame this whole pass exists to rescue) to be
/// discarded in favor of a guess.
///
/// Round 3 excluded only the cross-sequence-A7 path by its explicit
/// `via_cross_sequence_a7` marker; round 4 found a SECOND, separately-
/// marked speculative-decode mechanism (the within-window a7
/// cross-correlation pass) that path-specific check didn't cover, and
/// generalized to a payload-provenance check instead of adding a second
/// special case — see [`is_ineligible_for_hash_twin_removal`] for why
/// this is expected to also catch a not-yet-invented third mechanism
/// without needing its own follow-up.
///
/// Returns `(kept_messages, dropped_count)`. Order of the surviving
/// messages is preserved.
fn drop_unresolved_hash_twins(
    messages: Vec<pancetta_ft8::DecodedMessage>,
    protocol: pancetta_ft8::Protocol,
) -> (Vec<pancetta_ft8::DecodedMessage>, usize) {
    let freq_tolerance_hz = hash_twin_freq_tolerance_hz(protocol);
    let time_tolerance_s = hash_twin_time_tolerance_s(protocol);
    let symbol_period_s = pancetta_ft8::ProtocolParams::from_protocol(protocol).symbol_period;

    let n = messages.len();
    let mut drop = vec![false; n];

    for i in 0..n {
        if drop[i] {
            continue;
        }
        for j in (i + 1)..n {
            if drop[j] || messages[i].text == messages[j].text {
                continue;
            }
            if is_ineligible_for_hash_twin_removal(&messages[i])
                || is_ineligible_for_hash_twin_removal(&messages[j])
            {
                continue;
            }
            if (messages[i].frequency_offset - messages[j].frequency_offset).abs()
                > freq_tolerance_hz
            {
                continue;
            }
            let dt_i = normalized_time_offset(&messages[i], symbol_period_s);
            let dt_j = normalized_time_offset(&messages[j], symbol_period_s);
            if (dt_i - dt_j).abs() > time_tolerance_s {
                continue;
            }
            match hash_twin_keep_first(&messages[i].text, &messages[j].text) {
                Some(true) => drop[j] = true,
                Some(false) => {
                    drop[i] = true;
                    break; // i is gone; no point comparing it further
                }
                None => {}
            }
        }
    }

    let mut dropped_count = 0usize;
    let kept = messages
        .into_iter()
        .zip(drop)
        .filter_map(|(m, d)| {
            if d {
                dropped_count += 1;
                None
            } else {
                Some(m)
            }
        })
        .collect();
    (kept, dropped_count)
}

#[cfg(test)]
mod hash_twin_dedup_tests {
    use super::{drop_unresolved_hash_twins, hash_twin_keep_first};
    use pancetta_ft8::{ConfidenceFeatures, DecodedMessage, Ft8Message, Protocol};

    fn decoded_at(freq_hz: f64, text: &str) -> DecodedMessage {
        decoded_at_dt(freq_hz, 0.1, text)
    }

    fn decoded_at_dt(freq_hz: f64, time_offset: f64, text: &str) -> DecodedMessage {
        let mut d = DecodedMessage::new(Ft8Message::default(), -10.0, 0.5, freq_hz, time_offset);
        d.text = text.to_string();
        d
    }

    /// Marks a test-constructed message as a GENUINE native-pipeline
    /// decode (as opposed to the default ft8lib-FFI-origin every
    /// `decoded_at*` message starts as): gives it a `confidence_features`
    /// stamp exactly like the native decoder's `stamp_decode_origin`
    /// does, AND a non-empty `payload_bits` exactly like a real
    /// `MessageParser::parse_payload` round-trip does (PAN-40 round-4
    /// review's `native_decode_lacks_verified_payload` reads this field
    /// to tell a genuine decode apart from a template-derived one — a
    /// real native decode always has non-empty `payload_bits`, so this
    /// double must too or it would be wrongly classified as
    /// speculative). Needed to test
    /// [`super::normalized_time_offset`]'s asymmetric per-backend DT
    /// correction (PAN-40 round-3 review finding 1).
    fn native_origin(mut d: DecodedMessage) -> DecodedMessage {
        d.confidence_features = Some(ConfidenceFeatures {
            decode_origin: Some(0),
            ..Default::default()
        });
        d.message.payload_bits.resize(77, false);
        d
    }

    /// Marks a test-constructed message as a cross-sequence-A7
    /// speculative candidate (PAN-40 round-3 review finding 2) — the
    /// explicit `via_cross_sequence_a7` marker. `payload_bits` is left
    /// empty (the default), matching `try_cross_sequence_decodes`'s real
    /// construction via `Ft8Message::from_text`.
    fn a7_origin(mut d: DecodedMessage) -> DecodedMessage {
        d.via_cross_sequence_a7 = true;
        d
    }

    /// Marks a test-constructed message as a within-window a7
    /// cross-correlation candidate (`a7_cross_correlation_pass`,
    /// `decode_origin == 5`) — PAN-40 round-4 review's actual counter-
    /// example: native-origin, `decode_origin` stamped, but
    /// (deliberately, matching production) `payload_bits` left empty
    /// since it was built from the winning template's TEXT via
    /// `Ft8Message::from_text`, never a real LDPC/CRC decode. Crucially
    /// does NOT set `via_cross_sequence_a7` — that flag belongs to the
    /// DIFFERENT cross-sequence path; this is the separately-marked
    /// mechanism the round-3 fix didn't cover.
    fn within_window_a7_origin(mut d: DecodedMessage) -> DecodedMessage {
        d.confidence_features = Some(ConfidenceFeatures {
            decode_origin: Some(5),
            ..Default::default()
        });
        d
    }

    // --- hash_twin_keep_first (pure token-comparison unit) --------------

    #[test]
    fn keeps_resolved_side_over_unresolved_placeholder() {
        assert_eq!(
            hash_twin_keep_first("3B9/SQ9UM <...>", "3B9/SQ9UM <K5ARH>"),
            Some(false),
            "b (resolved) should be preferred"
        );
        assert_eq!(
            hash_twin_keep_first("3B9/SQ9UM <K5ARH>", "3B9/SQ9UM <...>"),
            Some(true),
            "a (resolved) should be preferred"
        );
    }

    #[test]
    fn genuinely_different_content_is_not_a_twin() {
        // Same shape (placeholder in the same slot) but a DIFFERENT
        // leading callsign entirely -- must not be treated as a twin.
        assert_eq!(
            hash_twin_keep_first("W2XYZ <...>", "3B9/SQ9UM <K5ARH>"),
            None
        );
    }

    #[test]
    fn different_token_counts_are_not_a_twin() {
        assert_eq!(
            hash_twin_keep_first("3B9/SQ9UM <...>", "3B9/SQ9UM <K5ARH> R-15"),
            None
        );
    }

    #[test]
    fn both_placeholders_is_ambiguous_keeps_both() {
        // Each side carries the unresolved placeholder at a DIFFERENT
        // position -- neither side is "more resolved" than the other,
        // so this must not pick a winner.
        assert_eq!(hash_twin_keep_first("<...> K1ABC", "K1ABC <...>"), None);
    }

    #[test]
    fn both_resolved_and_textually_different_is_not_a_twin() {
        assert_eq!(
            hash_twin_keep_first("3B9/SQ9UM <K5ARH>", "3B9/SQ9UM <W1AW>"),
            None
        );
    }

    // --- drop_unresolved_hash_twins (merge-list pass) --------------------

    #[test]
    fn drops_unresolved_twin_matching_freq_and_normalized_text() {
        let messages = vec![
            decoded_at(2700.0, "3B9/SQ9UM <...>"),
            decoded_at(2700.0, "3B9/SQ9UM <K5ARH>"),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(dropped, 1);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "3B9/SQ9UM <K5ARH>");
    }

    #[test]
    fn keeps_both_when_frequency_differs_beyond_tolerance() {
        let messages = vec![
            decoded_at(1200.0, "3B9/SQ9UM <...>"),
            decoded_at(2700.0, "3B9/SQ9UM <K5ARH>"),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn keeps_both_for_genuinely_different_signals_sharing_a_freq_bin() {
        let messages = vec![
            decoded_at(2700.0, "CQ K1ABC FN42"),
            decoded_at(2700.0, "W2XYZ K3DEF -05"),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 2);
    }

    #[test]
    fn no_placeholder_involved_and_identical_text_is_unaffected() {
        // The normal case today: both backends fully and identically
        // resolved the frame. This is the exact-text-dedup case (would
        // never reach this pass with two entries in practice, but the
        // pass itself must be a no-op on an already-single-copy list).
        let messages = vec![decoded_at(2700.0, "CQ K1ABC FN42")];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(dropped, 0);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "CQ K1ABC FN42");
    }

    #[test]
    fn three_way_merge_keeps_only_the_resolved_copy() {
        // ft8lib emits the unresolved placeholder, native emits the
        // resolved twin, AND an unrelated third decode on a different
        // frequency in the same window must survive untouched.
        let messages = vec![
            decoded_at(2700.0, "3B9/SQ9UM <...>"),
            decoded_at(1500.0, "CQ K1ABC FN42"),
            decoded_at(2700.0, "3B9/SQ9UM <K5ARH>"),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(dropped, 1);
        let texts: Vec<&str> = kept.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(texts, vec!["CQ K1ABC FN42", "3B9/SQ9UM <K5ARH>"]);
    }

    /// PAN-40 round-2 review (Codex finding 2), exact counter-example
    /// given: two DISTINCT callers replying to the same third party
    /// (`YS/WE9G`) in the same slot, at close-but-distinct frequencies.
    /// Post-render text alone can't tell these apart from a real hash
    /// twin -- frequency proximity must be strict enough to reject this.
    /// 6 Hz apart is within the OLD 10 Hz tolerance (would have wrongly
    /// merged) but outside the tightened one-bin (3.125 Hz) tolerance.
    #[test]
    fn close_but_distinct_frequencies_are_not_conflated() {
        let messages = vec![
            decoded_at(2700.0, "<...> YS/WE9G RR73"),
            decoded_at(2706.0, "<K1ABC> YS/WE9G RR73"),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(
            dropped, 0,
            "two different callers 6 Hz apart must not be conflated as a hash twin"
        );
        assert_eq!(kept.len(), 2);
    }

    /// PAN-40 round-2 review (Codex finding 2): same frequency bin is not
    /// enough on its own either -- two different callers can coincide in
    /// frequency. Their DT (arrival time within the window) essentially
    /// never coincidentally matches too, so the DT gate independently
    /// guards against this case.
    #[test]
    fn same_freq_bin_but_distinct_dt_is_not_conflated() {
        let messages = vec![
            decoded_at_dt(2700.0, 0.05, "<...> YS/WE9G RR73"),
            decoded_at_dt(2700.0, 0.65, "<K1ABC> YS/WE9G RR73"),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(
            dropped, 0,
            "same freq bin but 0.6s-apart DT must not be conflated as a hash twin"
        );
        assert_eq!(kept.len(), 2);
    }

    /// PAN-40 round-2 review (Codex finding 3): the decoder-merge summary
    /// log (`"FT8 decoder: {N} messages decoded ({ft8lib} ft8lib + ...")`)
    /// captures `ft8lib_count`/`native_added_count`/`cross_seq_recovered`
    /// — and logs against `decoded_messages.len()` — BEFORE this pass
    /// ever runs (see `drop_unresolved_hash_twins`'s doc comment: it must
    /// run after `partition_ap_eval_decodes`, which is itself well after
    /// that summary log). This test locks in the ordering invariant that
    /// makes the summary self-consistent: replaying the production
    /// coordinator's exact merge arithmetic (one ft8lib decode, one
    /// distinct-text native decode) shows the breakdown already sums to
    /// the pre-dedup total at the point the log fires, and this pass only
    /// shrinks the set LATER, after that number has already been logged
    /// -- so it can never retroactively invalidate it.
    #[test]
    fn merge_count_breakdown_sums_correctly_before_this_pass_ever_runs() {
        let ft8lib_messages = vec![decoded_at(2700.0, "3B9/SQ9UM <...>")];
        let native_messages = vec![decoded_at(2700.0, "3B9/SQ9UM <K5ARH>")];

        // Mirrors the real merge loop in `start_ft8_pipeline` exactly.
        let ft8lib_count = ft8lib_messages.len();
        let mut seen_texts: std::collections::HashSet<String> =
            ft8lib_messages.iter().map(|m| m.text.clone()).collect();
        let mut decoded_messages = ft8lib_messages;
        let mut native_added_count = 0usize;
        for msg in native_messages {
            if seen_texts.insert(msg.text.clone()) {
                decoded_messages.push(msg);
                native_added_count += 1;
            }
        }
        let cross_seq_recovered = 0usize; // no cross-sequence A7 in this scenario

        // This is the exact invariant the summary `info!` log depends on
        // at the moment it fires -- BEFORE hash-twin dedup has run.
        assert_eq!(
            ft8lib_count + native_added_count + cross_seq_recovered,
            decoded_messages.len(),
            "logged breakdown must sum to the logged total at log time"
        );
        assert_eq!(ft8lib_count, 1);
        assert_eq!(native_added_count, 1);
        assert_eq!(decoded_messages.len(), 2);

        // Only afterward (post-AP-partition, in production) does this
        // pass shrink the delivered set -- which is fine precisely
        // because the summary log already captured the correct numbers
        // for what was true at ITS point in the pipeline.
        let (kept, dropped) = drop_unresolved_hash_twins(decoded_messages, Protocol::Ft8);
        assert_eq!(dropped, 1);
        assert_eq!(kept.len(), 1);
    }

    /// PAN-40 round-3 review (Codex finding 1), the actual motivating
    /// regression: reproduce the TRUE per-backend DT conventions (not
    /// pre-normalized test fixtures) for the SAME physical candidate.
    /// ft8lib's raw DT is one symbol period (0.16 s for FT8) LATER than
    /// the native decoder's corrected DT for the identical signal (see
    /// `normalized_time_offset`'s doc comment) — prior to this fix that
    /// 0.16 s gap always exceeded the DT tolerance, so the motivating
    /// 3B9/SQ9UM unresolved/resolved pair was NEVER recognized as a
    /// twin and the live QSO starvation this whole PR exists to fix
    /// remained unfixed. This test proves the real-world case now
    /// passes.
    #[test]
    fn real_backend_dt_conventions_are_normalized_before_comparison() {
        const FT8_SYMBOL_PERIOD_S: f64 = 0.16;
        let native_true_dt = 0.34; // the corrected, true arrival time
        let ft8lib_raw_dt = native_true_dt + FT8_SYMBOL_PERIOD_S; // ft8lib's own (uncorrected) convention = 0.50

        let messages = vec![
            // ft8lib-origin (default `decoded_at_dt`): unresolved,
            // raw/late DT convention -- exactly what
            // `decode_window_ft8lib_protocol` reports.
            decoded_at_dt(2700.0, ft8lib_raw_dt, "3B9/SQ9UM <...>"),
            // native-origin: resolved, already-corrected DT convention
            // -- exactly what `candidate_offset_samples` reports.
            native_origin(decoded_at_dt(2700.0, native_true_dt, "3B9/SQ9UM <K5ARH>")),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(
            dropped, 1,
            "the real ft8lib/native twin must be recognized once DT conventions are normalized \
             onto a common basis before comparison"
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "3B9/SQ9UM <K5ARH>");
    }

    /// PAN-40 round-3 review (Codex finding 2): a cross-sequence-A7
    /// candidate is a SPECULATIVE template hypothesis, not a confirmed
    /// CRC-valid decode -- it must never be allowed to "win" a twin
    /// comparison and cause a real decode to be discarded in its favor.
    /// The real decode here still carries the unresolved hash
    /// placeholder (the exact case this whole pass exists to rescue);
    /// without the A7 exclusion this pair would look exactly like an
    /// ordinary hash twin (same freq/DT, texts differ only by the
    /// placeholder) and the A7 guess would wrongly displace the real
    /// decode.
    #[test]
    fn cross_sequence_a7_candidate_never_displaces_a_real_decode() {
        let messages = vec![
            // Real, CRC-valid decode from the standard pipeline -- still
            // carries the unresolved hash placeholder.
            decoded_at(2700.0, "<...> YS/WE9G RR73"),
            // Speculative A7 template hypothesis at the same freq/DT,
            // "resolving" the hash to a guessed callsign.
            a7_origin(decoded_at(2700.0, "<K1ABC> YS/WE9G RR73")),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(
            dropped, 0,
            "a cross-sequence-A7 candidate must never participate in hash-twin removal, \
             on either side of the pair"
        );
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter().any(|m| m.text == "<...> YS/WE9G RR73"),
            "the real CRC-valid decode must survive regardless of the A7 guess; kept={:?}",
            kept.iter().map(|m| m.text.as_str()).collect::<Vec<_>>()
        );
    }

    /// PAN-40 round-3 review (Codex finding 3): FT4's tone spacing
    /// (20.8333 Hz) and therefore its sub-bin frequency-estimate spacing
    /// (`tone_spacing / FREQ_OSR` ≈ 10.42 Hz) is much wider than FT8's
    /// (3.125 Hz). A near-boundary FT4 signal where the two backends'
    /// independent sync searches land in neighboring sub-bins can differ
    /// by nearly that whole ~10.42 Hz — the old FT8-tuned fixed 3.125 Hz
    /// tolerance would have rejected this pair as "too far apart" and
    /// left both the unresolved and resolved copies downstream. Proves
    /// the protocol-derived tolerance now recognizes it for FT4, and
    /// that the identical gap correctly does NOT merge under FT8 (where
    /// it exceeds the tighter, FT8-appropriate tolerance).
    #[test]
    fn ft4_near_boundary_hash_twin_is_recognized_with_protocol_scaled_tolerance() {
        let messages = vec![
            decoded_at(2700.0, "3B9/SQ9UM <...>"),
            // 10 Hz away: within FT4's ~10.42 Hz sub-bin tolerance,
            // outside FT8's 3.125 Hz one.
            decoded_at(2710.0, "3B9/SQ9UM <K5ARH>"),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft4);
        assert_eq!(
            dropped, 1,
            "an FT4 near-boundary twin 10 Hz apart must be recognized under the FT4-scaled \
             frequency tolerance"
        );
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].text, "3B9/SQ9UM <K5ARH>");

        // Sanity: the identical pair, evaluated as FT8, must NOT merge --
        // proves the tolerance is genuinely protocol-scoped, not just
        // loosened globally.
        let messages_ft8 = vec![
            decoded_at(2700.0, "3B9/SQ9UM <...>"),
            decoded_at(2710.0, "3B9/SQ9UM <K5ARH>"),
        ];
        let (kept_ft8, dropped_ft8) = drop_unresolved_hash_twins(messages_ft8, Protocol::Ft8);
        assert_eq!(
            dropped_ft8, 0,
            "the same 10 Hz gap must NOT merge under FT8's tighter (3.125 Hz) tolerance"
        );
        assert_eq!(kept_ft8.len(), 2);
    }

    /// PAN-40 round-4 review, the specific counter-example given: a
    /// within-window a7 hypothesis competing against a real, CRC-valid,
    /// still-unresolved decode of the same underlying signal. Without
    /// the generalized fix this pair looks exactly like an ordinary hash
    /// twin (same freq/DT, texts differ only by the placeholder), so the
    /// a7 guess would wrongly displace the real decode -- the round-3
    /// fix's `via_cross_sequence_a7`-only check didn't catch this
    /// SEPARATELY-marked mechanism (`decode_origin == 5`, no
    /// `via_cross_sequence_a7`).
    #[test]
    fn within_window_a7_candidate_never_displaces_a_real_decode() {
        let messages = vec![
            // Real, CRC-valid decode from the standard pipeline -- still
            // carries the unresolved hash placeholder.
            decoded_at(2700.0, "<...> YS/WE9G RR73"),
            // Speculative within-window a7 template hypothesis at the
            // same freq/DT, "resolving" the hash to a guessed callsign.
            within_window_a7_origin(decoded_at(2700.0, "<K1ABC> YS/WE9G RR73")),
        ];
        let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
        assert_eq!(
            dropped, 0,
            "a within-window a7 candidate must never participate in hash-twin removal, \
             even though it doesn't set via_cross_sequence_a7"
        );
        assert_eq!(kept.len(), 2);
        assert!(
            kept.iter().any(|m| m.text == "<...> YS/WE9G RR73"),
            "the real CRC-valid decode must survive regardless of the a7 guess; kept={:?}",
            kept.iter().map(|m| m.text.as_str()).collect::<Vec<_>>()
        );
    }

    /// PAN-40 round-4 review: proves the exclusion is truly UNIFIED, not
    /// one check per known `decode_origin` value. Every currently-
    /// assigned `decode_origin` ordinal (see
    /// `ConfidenceFeatures::decode_origin`'s doc comment in
    /// `pancetta-ft8/src/message.rs` -- ordinals 0-7 are documented
    /// there as "fully assigned") is exercised here with deliberately
    /// EMPTY `payload_bits` ("what if a future speculative mechanism
    /// stamped this ordinal"), and every single one must be excluded --
    /// because the check keys off `payload_bits`, never the ordinal
    /// value. If a NEW ordinal is ever added for a future decode pass,
    /// extending the range below is the obvious place to confirm it
    /// stays covered, instead of silently falling through like the
    /// round-3-to-round-4 regression did.
    #[test]
    fn every_known_decode_origin_ordinal_is_excluded_when_payload_is_unverified() {
        for origin in 0u8..=7 {
            let mut speculative = decoded_at(2700.0, "<K1ABC> YS/WE9G RR73");
            speculative.confidence_features = Some(ConfidenceFeatures {
                decode_origin: Some(origin),
                ..Default::default()
            });
            // payload_bits deliberately left empty -- this is the
            // property under test, independent of which ordinal is
            // stamped.
            assert!(
                speculative.message.payload_bits.is_empty(),
                "test setup sanity check for origin {origin}"
            );

            let messages = vec![decoded_at(2700.0, "<...> YS/WE9G RR73"), speculative];
            let (kept, dropped) = drop_unresolved_hash_twins(messages, Protocol::Ft8);
            assert_eq!(
                dropped, 0,
                "decode_origin={origin} with unverified payload_bits must be excluded from \
                 hash-twin removal regardless of which ordinal it carries"
            );
            assert_eq!(kept.len(), 2, "origin={origin}");
        }
    }
}

/// How many recent TX-clear ("quiet") windows feed the rolling decode-count
/// baseline for [`maybe_flag_tx_desense`].
const DESENSE_BASELINE_WINDOWS: usize = 20;
/// Minimum quiet-window samples collected before the baseline is trusted —
/// avoids flagging desense off a 1-2-sample fluke early in a session.
const DESENSE_MIN_BASELINE_SAMPLES: usize = 5;
/// Baseline mean below this is "the band's already quiet" — don't flag a
/// drop-to-near-zero as desense when there was barely anything to hear.
const DESENSE_MIN_BASELINE_MEAN: f64 = 3.0;
/// A window counts as "TX-adjacent" for this long after the last PTT-on.
/// Wider than one slot: 2026-07-04 on-air logs show the collapse persisting
/// into the very next (listen-only) window too, not just the TX window
/// itself.
const DESENSE_TX_ADJACENT_MS: u64 = 20_000;
/// Flag when the current window's decode count falls to this fraction (or
/// less) of the quiet-window baseline.
const DESENSE_DROP_RATIO: f64 = 0.25;

/// TX-adjacent decode-count desense check.
///
/// Operator report (2026-07-04): DX Hunter/Band Activity thin out to almost
/// nothing during an active QSO, and the station being called seems to
/// "vanish." Correlating on-air logs across 4 separate sessions showed FT8
/// sync-search quality (and therefore decoded-message counts) collapsing
/// band-wide for the exact duration of repeated keep-call transmissions,
/// recovering within one window after TX stopped — while audio RMS stayed
/// flat throughout. The parity/scheduling/relay code was reviewed and is
/// NOT the cause; this is most likely TX-induced desense or crosstalk in
/// the audio path (e.g. a shared TX/RX USB audio interface). This check
/// surfaces the pattern live instead of requiring a post-hoc log dig.
///
/// `decoded_count`/`now_ms` describe the just-finished window;
/// `last_ptt_on_ms` is 0 until the first-ever TX. `quiet_window_decode_counts`
/// and `desense_flagged` are caller-owned rolling state (one instance per
/// decode-loop lifetime). Returns `Some(warning text)` on the FIRST window of
/// a newly-detected episode only — edge-triggered so a multi-window collapse
/// (or a long run of keep-calls) warns once, not every ~15s.
fn maybe_flag_tx_desense(
    decoded_count: usize,
    now_ms: u64,
    last_ptt_on_ms: u64,
    quiet_window_decode_counts: &mut std::collections::VecDeque<usize>,
    desense_flagged: &mut bool,
) -> Option<String> {
    let tx_adjacent =
        last_ptt_on_ms != 0 && now_ms.saturating_sub(last_ptt_on_ms) < DESENSE_TX_ADJACENT_MS;

    if !tx_adjacent {
        // Only windows clear of our own TX contribute to the "healthy band"
        // baseline — a window during/just-after our own TX must never
        // pollute the very baseline it's being compared against.
        quiet_window_decode_counts.push_back(decoded_count);
        if quiet_window_decode_counts.len() > DESENSE_BASELINE_WINDOWS {
            quiet_window_decode_counts.pop_front();
        }
        *desense_flagged = false;
        return None;
    }

    if quiet_window_decode_counts.len() < DESENSE_MIN_BASELINE_SAMPLES {
        return None;
    }
    let baseline_mean: f64 = quiet_window_decode_counts.iter().sum::<usize>() as f64
        / quiet_window_decode_counts.len() as f64;
    if baseline_mean < DESENSE_MIN_BASELINE_MEAN {
        return None; // band's already quiet — nothing meaningful to compare against
    }

    let collapsed = (decoded_count as f64) <= baseline_mean * DESENSE_DROP_RATIO;
    if !collapsed {
        *desense_flagged = false;
        return None;
    }
    if *desense_flagged {
        return None; // already warned for this ongoing episode
    }
    *desense_flagged = true;
    Some(format!(
        "possible TX-induced desense: only {decoded_count} messages decoded this window vs. \
         a {baseline_mean:.1}-message recent baseline, within {DESENSE_TX_ADJACENT_MS}ms of our \
         last PTT — if this keeps happening, check TX/RX audio-path isolation (e.g. a shared \
         USB audio interface)"
    ))
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
        // TX-adjacent desense diagnostic input (see `maybe_flag_tx_desense`).
        let last_ptt_on_ms = self.last_ptt_on_ms.clone();
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

        // decoder-speed-overhaul Task 12: mode-aware decode deadline wiring.
        // `decode_effort_budget_ms` is the operator/preset-configured effort
        // (0 = unlimited; Task 14 is the first writer). Re-read once per
        // window alongside the slot/decode-phase atomics above so a live
        // config/TUI change (once Task 14 lands) takes effect on the very
        // next window. `decode_last_elapsed_ms`/`decode_last_budget_exhausted`
        // are the metrics counterparts written after each window's budgeted
        // decode calls and read by the TUI relay's periodic `PipelineHealth`
        // push.
        let decode_effort_budget_ms = self.decode_effort_budget_ms.clone();
        let decode_last_elapsed_ms = self.decode_last_elapsed_ms.clone();
        let decode_last_budget_exhausted = self.decode_last_budget_exhausted.clone();

        // Run FT8 decoder on a dedicated thread to avoid tokio starvation
        let handle = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Handle::current();
            info!("FT8 decoder thread started");

            // hb-216 S2: track the config tuple the current decoder was
            // built with. When the shared config changes (tier probe
            // landing a Slow preset), rebuild before the next decode.
            let mut last_max_passes = initial_ft8_config.max_decode_passes;
            let mut last_osd_depth = initial_ft8_config.osd_depth;
            let mut last_protocol = initial_ft8_config.protocol;

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

            // TX-adjacent desense diagnostic (see `maybe_flag_tx_desense`):
            // a rolling history of recent per-window decode counts, sampled
            // ONLY from windows well clear of our own transmit activity —
            // the "healthy band" baseline this station is actually hearing
            // right now, independent of time-of-day/propagation.
            let mut quiet_window_decode_counts: std::collections::VecDeque<usize> =
                std::collections::VecDeque::with_capacity(DESENSE_BASELINE_WINDOWS);
            let mut desense_flagged = false;

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
                        // at the end of the window.
                        let mut window_decode_elapsed_ms: u64 = 0;
                        let mut window_decode_budget_exhausted = false;

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

                        // decoder-speed-overhaul Task 12: mode-aware decode
                        // deadline for this window. `decode_effort_budget_ms
                        // == 0` (unlimited, the only value set anywhere
                        // today — Task 14 is the first writer) falls back to
                        // the ceiling alone; a nonzero operator/preset value
                        // is clamped to never exceed the ceiling. Shared by
                        // both decode call sites below so a single window
                        // has one consistent deadline.
                        let decode_budget = {
                            let cfg_ms = decode_effort_budget_ms.load(Ordering::Relaxed);
                            let ceiling_ms = decode_budget_ceiling_ms(slot_ns.max(0) as u64);
                            let budget_ms = if cfg_ms == 0 {
                                ceiling_ms
                            } else {
                                cfg_ms.min(ceiling_ms)
                            };
                            pancetta_ft8::DecodeBudget::until(
                                window_ready_at + std::time::Duration::from_millis(budget_ms),
                            )
                        };

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
                            let cur_protocol = cfg_guard.protocol;
                            cross_seq_enabled = cfg_guard.cross_sequence_a7_enabled;
                            if cur_max != last_max_passes
                                || cur_osd != last_osd_depth
                                || cur_protocol != last_protocol
                            {
                                let new_cfg = cfg_guard.clone();
                                drop(cfg_guard);
                                match Ft8Decoder::new(new_cfg) {
                                    Ok(mut d) => {
                                        info!(
                                            "FT8 decoder rebuilt: max_decode_passes={}, osd_depth={:?}, protocol={}",
                                            cur_max, cur_osd, cur_protocol
                                        );
                                        // PAN-17 round 2: a rebuild is a
                                        // fresh Ft8Decoder with an empty
                                        // hash table — re-seed our own
                                        // callsign or a compound-call DX's
                                        // reply silently stops resolving
                                        // after the next tier/protocol
                                        // config change.
                                        d.seed_hash_callsign(&station_callsign);
                                        // PAN-27 finding 2: also carry over
                                        // every OTHER caller this decoder
                                        // instance had already learned (the
                                        // round-2/round-4 seeding loops
                                        // below) — otherwise a compound
                                        // operator learned earlier this
                                        // session decodes as unresolved
                                        // again immediately after a rebuild,
                                        // even though the hash had already
                                        // resolved once.
                                        for call in decoder.learned_callsigns() {
                                            d.seed_hash_callsign(&call);
                                        }
                                        decoder = d;
                                        last_max_passes = cur_max;
                                        last_osd_depth = cur_osd;
                                        last_protocol = cur_protocol;
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
                                let partner_freq_pair: Option<(f64, Option<f64>)> =
                                    active_qso_freq_hz.read().ok().and_then(|g| *g);
                                if let Some((freq_hz, secondary_freq_hz)) = partner_freq_pair {
                                    let center = (freq_hz / 6.25).round() as usize;
                                    let mut lo = center.saturating_sub(SCOPED_HALF_WIDTH);
                                    let mut hi = center.saturating_add(SCOPED_HALF_WIDTH);
                                    if let Some(secondary_hz) = secondary_freq_hz {
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
                        let qso_freq_pair: Option<(f64, Option<f64>)> =
                            active_qso_freq_hz.read().ok().and_then(|g| *g);
                        let partner_freq_for_main = qso_freq_pair.map(|(primary, _)| primary);
                        let secondary_freq_for_main =
                            qso_freq_pair.and_then(|(_, secondary)| secondary);
                        let narrow_filter_bins =
                            super::qso_filter::compute_narrow_filter_bins_default_dual(
                                partner_freq_for_main,
                                secondary_freq_for_main,
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
                        // decodes (e.g. from AP injection) that ft8_lib missed
                        let ft8lib_count = ft8lib_messages.len();
                        let mut seen_texts: std::collections::HashSet<String> =
                            ft8lib_messages.iter().map(|m| m.text.clone()).collect();
                        let mut decoded_messages = ft8lib_messages;
                        let mut native_added_count = 0usize;
                        for msg in native_messages {
                            if seen_texts.insert(msg.text.clone()) {
                                decoded_messages.push(msg);
                                native_added_count += 1;
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

                        health_total_decodes
                            .fetch_add(decoded_messages.len() as u64, Ordering::Relaxed);

                        info!(
                            "FT8 decoder: {} messages decoded ({} ft8lib + {} native + {} cross-seq)",
                            decoded_messages.len(),
                            ft8lib_count,
                            native_added_count,
                            cross_seq_recovered,
                        );

                        if let Some(warning_text) = maybe_flag_tx_desense(
                            decoded_messages.len(),
                            super::now_epoch_ms(),
                            last_ptt_on_ms.load(Ordering::Relaxed),
                            &mut quiet_window_decode_counts,
                            &mut desense_flagged,
                        ) {
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
                        // filter judged false. The container is SHIPPED-INFRA
                        // — no consumer reads from it yet; downstream
                        // hypotheses will hook in here in future sessions.
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
// TX-adjacent desense diagnostic (2026-07-04 operator report)
// =============================================================================
#[cfg(test)]
mod desense_diagnostic_tests {
    use super::maybe_flag_tx_desense;
    use std::collections::VecDeque;

    /// Fresh caller-owned state, mirroring what the decode loop holds.
    fn state() -> (VecDeque<usize>, bool) {
        (VecDeque::new(), false)
    }

    /// Fill the baseline with `n` quiet (non-TX-adjacent) windows of
    /// `count` decodes each. `last_ptt_on_ms = 0` means "never transmitted".
    fn fill_baseline(counts: &mut VecDeque<usize>, flagged: &mut bool, n: usize, count: usize) {
        for _ in 0..n {
            maybe_flag_tx_desense(count, 100_000, 0, counts, flagged);
        }
    }

    #[test]
    fn no_flag_with_insufficient_baseline_history() {
        let (mut counts, mut flagged) = state();
        // Only 2 quiet samples — below DESENSE_MIN_BASELINE_SAMPLES (5).
        fill_baseline(&mut counts, &mut flagged, 2, 20);
        let now = 200_000u64;
        let last_ptt = now - 5_000; // well within the TX-adjacent window
        let warning = maybe_flag_tx_desense(0, now, last_ptt, &mut counts, &mut flagged);
        assert!(
            warning.is_none(),
            "must not flag before the baseline has enough samples"
        );
    }

    #[test]
    fn no_flag_when_band_is_already_quiet() {
        let (mut counts, mut flagged) = state();
        // Baseline itself is near-zero (a genuinely dead band) — dropping
        // further to 0 is not "desense", there was nothing to lose.
        fill_baseline(&mut counts, &mut flagged, 10, 1);
        let now = 200_000u64;
        let last_ptt = now - 5_000;
        let warning = maybe_flag_tx_desense(0, now, last_ptt, &mut counts, &mut flagged);
        assert!(
            warning.is_none(),
            "must not flag when the baseline itself is below the quiet-band floor"
        );
    }

    #[test]
    fn no_flag_when_not_tx_adjacent() {
        let (mut counts, mut flagged) = state();
        fill_baseline(&mut counts, &mut flagged, 10, 20);
        let now = 200_000u64;
        // last_ptt_on_ms far in the past — outside DESENSE_TX_ADJACENT_MS.
        let last_ptt = now - 60_000;
        let warning = maybe_flag_tx_desense(0, now, last_ptt, &mut counts, &mut flagged);
        assert!(
            warning.is_none(),
            "a decode-count drop with no recent TX must not be flagged as desense"
        );
    }

    #[test]
    fn no_flag_when_never_transmitted() {
        let (mut counts, mut flagged) = state();
        fill_baseline(&mut counts, &mut flagged, 10, 20);
        // last_ptt_on_ms == 0 is the "never transmitted" sentinel.
        let warning = maybe_flag_tx_desense(0, 200_000, 0, &mut counts, &mut flagged);
        assert!(warning.is_none());
    }

    #[test]
    fn flags_collapse_during_tx_adjacent_window() {
        let (mut counts, mut flagged) = state();
        fill_baseline(&mut counts, &mut flagged, 10, 20);
        let now = 200_000u64;
        let last_ptt = now - 5_000; // TX-adjacent
        let warning = maybe_flag_tx_desense(1, now, last_ptt, &mut counts, &mut flagged);
        assert!(
            warning.is_some(),
            "a collapse to 1/20th of baseline during a TX-adjacent window must be flagged"
        );
        assert!(warning.unwrap().contains("desense"));
    }

    #[test]
    fn does_not_flag_a_mild_dip() {
        let (mut counts, mut flagged) = state();
        fill_baseline(&mut counts, &mut flagged, 10, 20);
        let now = 200_000u64;
        let last_ptt = now - 5_000;
        // 8/20 = 40%, above the 25% drop-ratio threshold.
        let warning = maybe_flag_tx_desense(8, now, last_ptt, &mut counts, &mut flagged);
        assert!(warning.is_none(), "a mild dip must not trigger the warning");
    }

    #[test]
    fn edge_triggered_only_warns_once_per_episode_then_rearms_after_recovery() {
        let (mut counts, mut flagged) = state();
        fill_baseline(&mut counts, &mut flagged, 10, 20);
        let now = 200_000u64;
        let last_ptt = now - 5_000;

        let first = maybe_flag_tx_desense(0, now, last_ptt, &mut counts, &mut flagged);
        assert!(first.is_some(), "first collapsed window warns");

        let second = maybe_flag_tx_desense(0, now + 15_000, last_ptt, &mut counts, &mut flagged);
        assert!(
            second.is_none(),
            "a still-collapsed subsequent window must not re-warn"
        );

        // Recovery: a non-TX-adjacent, healthy window clears the flag AND
        // re-seeds the baseline.
        let recovered = maybe_flag_tx_desense(20, now + 30_000, 0, &mut counts, &mut flagged);
        assert!(recovered.is_none());

        // A second, later collapse must warn again (not permanently suppressed).
        let last_ptt2 = now + 40_000;
        let third = maybe_flag_tx_desense(0, now + 45_000, last_ptt2, &mut counts, &mut flagged);
        assert!(
            third.is_some(),
            "a fresh episode after recovery must warn again"
        );
    }

    #[test]
    fn quiet_window_baseline_is_bounded() {
        let (mut counts, mut flagged) = state();
        // Push far more than DESENSE_BASELINE_WINDOWS (20) quiet samples.
        fill_baseline(&mut counts, &mut flagged, 100, 20);
        assert!(
            counts.len() <= 20,
            "quiet-window history must stay capped, not grow unbounded over a session"
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
