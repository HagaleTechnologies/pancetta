//! Production's FT8/FT4 decode-set merge (PAN-110).
//!
//! The coordinator (`pancetta/src/coordinator/ft8.rs`) runs the ft8_lib
//! reference decoder and the native decoder on every window, merges the two
//! result sets with [`merge_ft8lib_first`], and drops unresolved-hash twins
//! from the delivered set with [`drop_unresolved_hash_twins`]. Both live
//! here, rather than in the coordinator, so the research harness
//! (`pancetta-research`, which does not depend on the `pancetta` crate)
//! scores the exact merge that ships.

use std::collections::HashSet;

/// Result of [`merge_ft8lib_first`].
#[derive(Debug, Clone, Default)]
pub struct MergedDecodes {
    /// ft8_lib's decodes in their original order, followed by each native
    /// decode whose text ft8_lib did not produce.
    pub messages: Vec<crate::DecodedMessage>,
    /// Every decode text in `messages`. Later stages (the coordinator's
    /// cross-sequence A7 pass) extend it to dedup their own additions.
    pub seen_texts: HashSet<String>,
    /// How many of `messages` came from ft8_lib.
    pub ft8lib_count: usize,
    /// How many native decodes were added after ft8_lib's.
    pub native_added: usize,
}

/// Merge one window's ft8_lib and native decodes the way production does:
/// start with ft8_lib's results, then add any native decode (e.g. one from
/// AP injection) whose text ft8_lib missed. Dedup is by exact message text.
pub fn merge_ft8lib_first(
    ft8lib: Vec<crate::DecodedMessage>,
    native: Vec<crate::DecodedMessage>,
) -> MergedDecodes {
    let ft8lib_count = ft8lib.len();
    let mut seen_texts: HashSet<String> = ft8lib.iter().map(|m| m.text.clone()).collect();
    let mut messages = ft8lib;
    let mut native_added = 0usize;
    for msg in native {
        if seen_texts.insert(msg.text.clone()) {
            messages.push(msg);
            native_added += 1;
        }
    }
    MergedDecodes {
        messages,
        seen_texts,
        ft8lib_count,
        native_added,
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
/// ≈ 10.42 Hz` (`crate::ProtocolParams::ft4`). `FREQ_OSR` itself
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
fn hash_twin_freq_tolerance_hz(protocol: crate::Protocol) -> f64 {
    const FREQ_OSR: f64 = 2.0;
    crate::ProtocolParams::from_protocol(protocol).tone_spacing / FREQ_OSR
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
fn hash_twin_time_tolerance_s(protocol: crate::Protocol) -> f64 {
    crate::ProtocolParams::from_protocol(protocol).symbol_period * 0.625
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
fn is_ft8lib_origin_decode(msg: &crate::DecodedMessage) -> bool {
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
fn normalized_time_offset(msg: &crate::DecodedMessage, symbol_period_s: f64) -> f64 {
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
fn native_decode_lacks_verified_payload(msg: &crate::DecodedMessage) -> bool {
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
fn is_ineligible_for_hash_twin_removal(msg: &crate::DecodedMessage) -> bool {
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
pub fn drop_unresolved_hash_twins(
    messages: Vec<crate::DecodedMessage>,
    protocol: crate::Protocol,
) -> (Vec<crate::DecodedMessage>, usize) {
    let freq_tolerance_hz = hash_twin_freq_tolerance_hz(protocol);
    let time_tolerance_s = hash_twin_time_tolerance_s(protocol);
    let symbol_period_s = crate::ProtocolParams::from_protocol(protocol).symbol_period;

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
    use crate::{ConfidenceFeatures, DecodedMessage, Ft8Message, Protocol};

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

#[cfg(test)]
mod merge_ft8lib_first_tests {
    use super::merge_ft8lib_first;
    use crate::{DecodedMessage, Ft8Message};

    fn decoded(freq_hz: f64, text: &str) -> DecodedMessage {
        let mut d = DecodedMessage::new(Ft8Message::default(), -10.0, 0.5, freq_hz, 0.1);
        d.text = text.to_string();
        d
    }

    fn texts(messages: &[DecodedMessage]) -> Vec<&str> {
        messages.iter().map(|m| m.text.as_str()).collect()
    }

    #[test]
    fn ft8lib_order_is_preserved_first() {
        let ft8lib = vec![
            decoded(1500.0, "CQ K1ABC FN42"),
            decoded(900.0, "W9XYZ K1ABC -12"),
        ];
        let native = vec![decoded(2100.0, "CQ DL1AAA JO62")];
        let merged = merge_ft8lib_first(ft8lib, native);
        assert_eq!(
            texts(&merged.messages),
            ["CQ K1ABC FN42", "W9XYZ K1ABC -12", "CQ DL1AAA JO62"]
        );
        assert_eq!(merged.ft8lib_count, 2);
        assert_eq!(merged.native_added, 1);
    }

    #[test]
    fn native_decode_with_text_ft8lib_already_produced_is_skipped() {
        let ft8lib = vec![decoded(1500.0, "CQ K1ABC FN42")];
        // Same text, different frequency: dedup is by text alone, and the
        // ft8_lib copy wins.
        let native = vec![
            decoded(1503.0, "CQ K1ABC FN42"),
            decoded(700.0, "CQ K1ABC FN42"),
        ];
        let merged = merge_ft8lib_first(ft8lib, native);
        assert_eq!(texts(&merged.messages), ["CQ K1ABC FN42"]);
        assert_eq!(merged.messages[0].frequency_offset, 1500.0);
        assert_eq!(merged.native_added, 0);
    }

    /// Replays the merge loop the coordinator ran inline before PAN-110 and
    /// checks the shared function produces the same set, counts and
    /// `seen_texts`.
    #[test]
    fn counts_and_seen_texts_match_the_former_inline_merge() {
        let ft8lib = vec![decoded(1500.0, "A"), decoded(1600.0, "B")];
        let native = vec![
            decoded(1500.0, "A"),
            decoded(1700.0, "C"),
            decoded(1800.0, "C"),
            decoded(1900.0, "D"),
        ];

        let ft8lib_count = ft8lib.len();
        let mut seen_texts: std::collections::HashSet<String> =
            ft8lib.iter().map(|m| m.text.clone()).collect();
        let mut decoded_messages = ft8lib.clone();
        let mut native_added_count = 0usize;
        for msg in native.clone() {
            if seen_texts.insert(msg.text.clone()) {
                decoded_messages.push(msg);
                native_added_count += 1;
            }
        }

        let merged = merge_ft8lib_first(ft8lib, native);
        assert_eq!(texts(&merged.messages), texts(&decoded_messages));
        assert_eq!(merged.ft8lib_count, ft8lib_count);
        assert_eq!(merged.native_added, native_added_count);
        assert_eq!(merged.native_added, 2);
        assert_eq!(merged.seen_texts, seen_texts);
        assert_eq!(
            merged.ft8lib_count + merged.native_added,
            merged.messages.len()
        );
    }
}
