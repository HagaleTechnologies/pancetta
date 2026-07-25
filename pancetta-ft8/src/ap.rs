//! A Priori (AP) decoding support for FT8.
//!
//! Provides AP context types and LLR injection for AP-enhanced decoding.
//! AP decoding uses known information (own callsign, active QSO partner)
//! to improve decode success at low SNR by injecting high-confidence
//! LLR values at known bit positions in the 77-bit FT8 payload.
//!
//! FT8 77-bit standard-message (i3=1/2) payload layout (per
//! `message.rs::parse_type1_standard`, the ground truth): `n29a` occupies
//! bits 0-28 = `to_callsign` (28-bit packed value at bits 0-27, `/P`-suffix
//! flag at bit 28); `n29b` occupies bits 29-57 = `from_callsign` (28-bit
//! packed value at bits 29-56, suffix flag at bit 57); bits 58-76 are
//! ir/grid/report/i3.
//!
//! We are always the message's addressee for anything we decode (we never
//! decode our own transmissions), so **our callsign is always
//! `to_callsign` (bits 0-27)** and the other station is always
//! `from_callsign` (bits 29-56) — there is a 1-bit gap at bit 28 (the
//! `to_callsign` suffix flag) between the two 28-bit callsign fields, so
//! they are NOT evenly spaced at offsets 0 and 28.
//!
//! - Bits 0-27:  to_callsign / called station (us) — 28 bits
//! - Bit 28:     to_callsign suffix flag (`/P`/`/R`)
//! - Bits 29-56: from_callsign / calling station (the other station) — 28 bits
//! - Bit 57:     from_callsign suffix flag
//! - Bits 58-76: ir + grid/report + i3 (19 bits)

#![allow(dead_code)]
// rationale: AP LLR-injection loops index the 77-bit payload positions; the
// index is load-bearing for the protocol bit layout.
#![allow(clippy::needless_range_loop)]

/// High-confidence LLR magnitude for known AP bits.
const AP_LLR_MAGNITUDE: f32 = 15.0;

/// WSJT-X constants for callsign encoding (same as encoder.rs)
const NTOKENS: u32 = 2_063_592;
const MAX22: u32 = 4_194_304;

// ---------------------------------------------------------------------------
// Standalone pack28 (avoids dependency on transmit-gated encoder module)
// ---------------------------------------------------------------------------

/// Pack a callsign into a 28-bit integer, matching WSJT-X encoding.
/// Returns `(packed_value, suffix_flag)` or `None` on failure.
fn pack28(callsign: &str) -> Option<(u32, u8)> {
    // Special tokens
    match callsign {
        "DE" => return Some((0, 0)),
        "QRZ" => return Some((1, 0)),
        "CQ" => return Some((2, 0)),
        _ => {}
    }

    // CQ with modifier
    if callsign.starts_with("CQ ") && callsign.len() < 8 {
        let modifier = &callsign[3..];
        if let Some(v) = parse_cq_modifier(modifier) {
            return Some((3 + v, 0));
        }
        return None;
    }

    // Detect /R or /P suffix
    let (base, ip) = if callsign.ends_with("/P") || callsign.ends_with("/R") {
        (&callsign[..callsign.len() - 2], 1u8)
    } else {
        (callsign, 0u8)
    };

    let n28 = pack_basecall(base)?;
    Some((NTOKENS + MAX22 + n28, ip))
}

fn parse_cq_modifier(modifier: &str) -> Option<u32> {
    if modifier.is_empty() || modifier.len() > 4 {
        return None;
    }
    let bytes = modifier.as_bytes();
    let all_digits = bytes.iter().all(|b| b.is_ascii_digit());
    let all_letters = bytes.iter().all(|b| b.is_ascii_uppercase());

    if all_digits && modifier.len() == 3 {
        let nnn: u32 = modifier.parse().ok()?;
        Some(nnn)
    } else if all_letters && modifier.len() <= 4 {
        let mut m: u32 = 0;
        for &b in bytes {
            m = 27 * m + ((b - b'A') as u32 + 1);
        }
        Some(1000 + m)
    } else {
        None
    }
}

fn pack_basecall(callsign: &str) -> Option<u32> {
    let length = callsign.len();
    if !(3..=6).contains(&length) {
        return None;
    }
    let bytes = callsign.as_bytes();
    let mut c6 = [b' '; 6];

    if callsign.starts_with("3DA0") && length > 4 && length <= 7 {
        c6[0] = b'3';
        c6[1] = b'D';
        c6[2] = b'0';
        for (i, &b) in bytes[4..].iter().enumerate() {
            if i + 3 < 6 {
                c6[i + 3] = b;
            }
        }
    } else if callsign.starts_with("3X")
        && length > 2
        && bytes[2].is_ascii_alphabetic()
        && length <= 7
    {
        c6[0] = b'Q';
        for (i, &b) in bytes[2..].iter().enumerate() {
            if i + 1 < 6 {
                c6[i + 1] = b;
            }
        }
    } else if length >= 3 && bytes[2].is_ascii_digit() && length <= 6 {
        c6[..length].copy_from_slice(&bytes[..length]);
    } else if length >= 2 && bytes[1].is_ascii_digit() && length <= 5 {
        c6[1..1 + length].copy_from_slice(&bytes[..length]);
    } else {
        return None;
    }

    let i0 = nchar_alphanum_space(c6[0])?;
    let i1 = nchar_alphanum(c6[1])?;
    let i2 = nchar_numeric(c6[2])?;
    let i3 = nchar_letters_space(c6[3])?;
    let i4 = nchar_letters_space(c6[4])?;
    let i5 = nchar_letters_space(c6[5])?;

    let mut n: u32 = i0;
    n = n * 36 + i1;
    n = n * 10 + i2;
    n = n * 27 + i3;
    n = n * 27 + i4;
    n = n * 27 + i5;
    Some(n)
}

fn nchar_alphanum_space(c: u8) -> Option<u32> {
    match c {
        b' ' => Some(0),
        b'0'..=b'9' => Some((c - b'0') as u32 + 1),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 11),
        _ => None,
    }
}

fn nchar_alphanum(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 10),
        _ => None,
    }
}

fn nchar_numeric(c: u8) -> Option<u32> {
    match c {
        b'0'..=b'9' => Some((c - b'0') as u32),
        _ => None,
    }
}

fn nchar_letters_space(c: u8) -> Option<u32> {
    match c {
        b' ' => Some(0),
        b'A'..=b'Z' => Some((c - b'A') as u32 + 1),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Bit helpers
// ---------------------------------------------------------------------------

/// Convert a 28-bit packed value to a bool array, MSB first.
pub fn u32_to_bits_28(value: u32) -> [bool; 28] {
    let mut bits = [false; 28];
    for i in 0..28 {
        bits[i] = (value >> (27 - i)) & 1 == 1;
    }
    bits
}

/// Convert a 15-bit packed value to a bool array, MSB first. Used for the
/// `igrid4` field (payload bits 59-73).
pub fn u16_to_bits_15(value: u16) -> [bool; 15] {
    let mut bits = [false; 15];
    for i in 0..15 {
        bits[i] = (value >> (14 - i)) & 1 == 1;
    }
    bits
}

// ---------------------------------------------------------------------------
// AP types
// ---------------------------------------------------------------------------

/// AP level controlling how much a priori information is injected.
///
/// Not `Copy`: `Ap5` carries an owned [`ContentHypothesis`] (a `String` +
/// bit array), so this enum can no longer be bitwise-duplicated implicitly.
/// Every call site that used to rely on an implicit copy now clones
/// explicitly — cheap for the unit variants (Ap0-Ap4, Cq), and only
/// meaningfully allocates for Ap5, which isn't wired into any decode-loop
/// hot path yet (see Ap5's doc comment).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApLevel {
    /// No AP injection.
    Ap0,
    /// Inject own callsign at bits 0-27 (called station / `to_callsign`
    /// — we are always the addressee for any message we decode).
    Ap1,
    /// Inject a recent caller's callsign at bits 29-56 (calling station /
    /// `from_callsign`). The specific caller is selected externally via
    /// `inject_ap2_caller`.
    Ap2,
    /// Inject both: own call at bits 0-27 (`to_callsign`), active QSO
    /// partner at bits 29-56 (`from_callsign`).
    Ap3,
    /// AP3 + inject i3 type bits (74-76) as 0,0,1 (i3=1, the "standard
    /// message" family RR73/RRR/73 actually use).
    Ap4,
    /// Ap3 (own call at 0-27, partner at 29-56) + inject one specific
    /// enumerated content hypothesis's bits 58-76 (report/token + type).
    /// Soft injection, same ±AP_LLR_MAGNITUDE convention as every other
    /// level -- LDPC/CRC can still override a wrong hypothesis, which is
    /// exactly what makes the survival check (`ap_injection_survived`'s
    /// `Ap5` arm) meaningful. Unlike Ap0-Ap4 and `Cq`, which are unit
    /// variants selected once per candidate, `Ap5` needs per-attempt
    /// data (which hypothesis is being tried this pass), so it carries
    /// the [`ContentHypothesis`] itself.
    Ap5(ContentHypothesis),
    /// Decoder-TP-sensitivity Task W2.6 [A/B]: assume this candidate is a
    /// plain "CQ" call. Injects the `to_callsign` field (bits 0-27 + the
    /// bit-28 suffix flag) with the packed "CQ" special token (`pack28`
    /// value 2, matching this project's own encoder's `try_encode_standard`
    /// / `parse_standard_message`) plus the i3 type bits (74-76) as
    /// (0,0,1) — the same "standard message" family AP4 assumes. Unlike
    /// AP1-AP4, this level needs **no context at all**: "CQ" is a fixed
    /// protocol token, not a personal callsign, so it requires neither
    /// `ApContext.my_call` nor `ApContext.active_qso` and can run
    /// unconditionally on every candidate. Scope: plain "CQ" only (not
    /// "CQ DX"/"CQ POTA"/etc modifiers, which pack a different token
    /// value).
    Cq,
}

/// Decoder-TP-sensitivity Task W2.6 [A/B]: which QSO-completion token to
/// inject the full message-content mask for, on top of the existing AP4
/// callsign + i3 injection. AP4 alone only pins the message-TYPE (i3=1);
/// it never pinned the specific completion content (the `ir` bit and the
/// 15-bit `igrid4` field that actually spells out RRR/RR73/73). Mirrors
/// `encoder.rs::packgrid` / `message.rs::unpackgrid`'s special
/// `MAXGRID4`-relative values (`MAXGRID4 = 32400`); ground truth verified
/// against this project's own encoder in
/// `pancetta-ft8/tests/ap_i3_tests.rs`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfirmationToken {
    /// "RRR" — igrid4 = MAXGRID4 + 2.
    Rrr,
    /// "RR73" — igrid4 = MAXGRID4 + 3.
    RR73,
    /// "73" — igrid4 = MAXGRID4 + 4.
    Final73,
}

impl ConfirmationToken {
    /// The three canonical confirmation tokens, RR73 first (by far the
    /// most common single completion message in real operator use —
    /// combines both the "roger" and "73" acknowledgements in one
    /// transmission), then RRR, then bare 73.
    pub const ALL: [ConfirmationToken; 3] = [
        ConfirmationToken::RR73,
        ConfirmationToken::Rrr,
        ConfirmationToken::Final73,
    ];

    /// The 15-bit `igrid4` field value this token packs to.
    fn igrid4_value(self) -> u16 {
        const MAXGRID4: u16 = 32400;
        match self {
            ConfirmationToken::Rrr => MAXGRID4 + 2,
            ConfirmationToken::RR73 => MAXGRID4 + 3,
            ConfirmationToken::Final73 => MAXGRID4 + 4,
        }
    }
}

/// QSO progress within an active AP-tracked contact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QsoApProgress {
    /// Waiting for a signal report from the other station.
    WaitingForReport,
    /// Waiting for confirmation (RR73 / RRR / 73).
    WaitingForConfirmation,
}

/// Own callsign encoded for AP injection.
#[derive(Debug, Clone)]
pub struct MyCallAp {
    pub callsign: String,
    pub packed_28: u32,
    pub bits: [bool; 28],
}

impl MyCallAp {
    /// Create from a callsign string. Returns `None` if the callsign cannot be
    /// encoded with the standard 28-bit packing.
    pub fn new(callsign: &str) -> Option<Self> {
        let (packed, _ip) = pack28(callsign)?;
        Some(Self {
            callsign: callsign.to_string(),
            packed_28: packed,
            bits: u32_to_bits_28(packed),
        })
    }
}

/// A recently-heard callsign, used for AP2 injection.
#[derive(Debug, Clone)]
pub struct RecentCallAp {
    pub callsign: String,
    pub packed_28: u32,
    pub bits: [bool; 28],
    pub last_snr: f32,
}

impl RecentCallAp {
    /// Create from a callsign and its last observed SNR.
    /// Returns `None` if the callsign cannot be encoded.
    pub fn new(callsign: &str, snr: f32) -> Option<Self> {
        let (packed, _ip) = pack28(callsign)?;
        Some(Self {
            callsign: callsign.to_string(),
            packed_28: packed,
            bits: u32_to_bits_28(packed),
            last_snr: snr,
        })
    }
}

/// Active QSO context for AP3/AP4 injection.
///
/// `expected_next_message_texts` carries the small enumerated list of
/// messages we expect to receive from the partner in the *next* slot,
/// given the operator's current QSO state. Used by the a8
/// sequenced-QSO-state AP path (see [`enumerate_a8_expected_texts`])
/// to relax the AP confidence gate for decodes that match the
/// pre-enumerated templates. Empty when a8 enumeration was not
/// performed (or wasn't applicable for this state).
#[derive(Debug, Clone)]
pub struct QsoAp {
    pub their_call: String,
    pub their_packed_28: u32,
    pub their_bits: [bool; 28],
    pub progress: QsoApProgress,
    /// a8 sequenced-QSO-state AP candidate set: small list of expected
    /// next partner messages (canonical FT8 text, e.g.
    /// "K1ABC W1AW RR73"). Empty list means "no a8 enumeration available"
    /// — the decoder treats the QsoAp the same as the legacy AP3/AP4
    /// path. Populated by the coordinator via
    /// [`enumerate_a8_expected_texts`].
    pub expected_next_message_texts: Vec<String>,
}

impl QsoAp {
    /// Create from the other station's callsign and current QSO progress.
    /// Returns `None` if the callsign cannot be encoded.
    ///
    /// `expected_next_message_texts` starts empty. The coordinator may
    /// populate it via [`QsoAp::with_expected_texts`] after construction.
    pub fn new(their_call: &str, progress: QsoApProgress) -> Option<Self> {
        let (packed, _ip) = pack28(their_call)?;
        Some(Self {
            their_call: their_call.to_string(),
            their_packed_28: packed,
            their_bits: u32_to_bits_28(packed),
            progress,
            expected_next_message_texts: Vec::new(),
        })
    }

    /// Builder-style helper to attach the a8 expected-message templates.
    /// Drops empty entries and uppercases each text for canonical match.
    pub fn with_expected_texts<I, S>(mut self, texts: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.expected_next_message_texts = texts
            .into_iter()
            .map(|s| s.into().trim().to_uppercase())
            .filter(|s| !s.is_empty())
            .collect();
        self
    }
}

/// Build the a8 sequenced-QSO-state AP candidate list for the partner's
/// expected *next* message.
///
/// Inspired by spec ref `spec-wsjtx-improved-a8-decoding.md` —
/// WSJT-X Improved (DG2YCB) a8 enumerates the small set of legal next
/// partner messages given the operator's current QSO state. Pancetta's
/// adaptation: text-template enumeration, used by the decoder as a
/// confidence-gate relaxation when an AP3/AP4 decode matches one of
/// the templates.
///
/// Returns an empty `Vec` when enumeration is not applicable
/// (callsign too long, state has no canonical next-message family,
/// etc.). The coordinator passes the result to
/// [`QsoAp::with_expected_texts`].
///
/// Notes
/// - All texts are uppercase, single-space separated, with **our own
///   call** (`my_call`) as the first token and the partner's call
///   (`dx_call`) second — matching `Ft8Message::Display`'s
///   `to_callsign from_callsign ...` text order: any message the
///   partner sends *to us* has us as the addressee (first token), per
///   `message.rs::parse_type1_standard`'s bit layout (`to_callsign` at
///   bits 0-27, `from_callsign` at bits 29-56).
/// - The enumerations are intentionally small (≤6 entries per state);
///   they exist to *gate*, not to *seed* LDPC.
pub fn enumerate_a8_expected_texts(
    my_call: &str,
    dx_call: &str,
    progress: QsoApProgress,
) -> Vec<String> {
    let my = my_call.trim().to_uppercase();
    let dx = dx_call.trim().to_uppercase();
    if my.is_empty() || dx.is_empty() {
        return Vec::new();
    }

    match progress {
        // Operator has sent the partner a grid/report; partner is
        // expected to reply with either a signal report (-NN) or a
        // confirmed report (R-NN). Enumerate the canonical SNR range
        // [-22 .. 0 dB] in 2 dB steps, both R- and bare variants. The
        // table is small — ~24 entries — and covers >90% of real
        // operator behavior.
        QsoApProgress::WaitingForReport => {
            let mut out = Vec::with_capacity(24);
            let mut snr = -22i32;
            while snr <= 0 {
                out.push(format!("{} {} R{:+03}", my, dx, snr));
                out.push(format!("{} {} {:+03}", my, dx, snr));
                snr += 2;
            }
            out
        }
        // Operator has acknowledged the partner's report; partner is
        // expected to reply with a confirmation. Three canonical
        // confirmation tokens.
        QsoApProgress::WaitingForConfirmation => {
            vec![
                format!("{} {} RR73", my, dx),
                format!("{} {} 73", my, dx),
                format!("{} {} RRR", my, dx),
            ]
        }
    }
}

// ---------------------------------------------------------------------------
// Content-hypothesis bit builder (ap-decoding-design.md gap 1: content
// bits are enumerated as text but never turned into an LLR-injectable
// bit pattern)
// ---------------------------------------------------------------------------

/// First payload bit of the AP5 content field. Per
/// `message.rs::parse_type1_standard` (ground truth: `n29a(29) +
/// n29b(29) + ir(1) + igrid4(15) + i3(3) = 77`), `from_callsign`'s 28-bit
/// packed value occupies bits 29-56 and its suffix flag is bit 57, so the
/// content field (`ir` + `igrid4` + `i3`) starts at bit 58, not 56 — see
/// [`ContentHypothesis`]'s doc for the full correction vs the originating
/// plan prose.
const CONTENT_FIELD_START_BIT: usize = 58;

/// Length in bits of the AP5 content field: `ir`(1) + `igrid4`(15) +
/// `i3`(3) = 19.
const CONTENT_FIELD_LEN: usize = 19;

/// One enumerated content hypothesis, ready for Ap5 injection: the source
/// text (for logging/content-match verification) plus its extracted
/// content-field bit pattern.
///
/// **Bit-range correction vs the originating plan prose**
/// (`docs/superpowers/plans/2026-07-25-ap-content-decoding.md`, and
/// `docs/ap-decoding-design.md`): those describe "content bits 56-76", a
/// 21-bit field. That count predates this project's own W1.7
/// callsign-bit-offset fix (2026-07-07 — see this module's top-of-file
/// doc comment and `pancetta-ft8/tests/ap_i3_tests.rs`) and doesn't
/// account for the two 1-bit callsign-suffix-flag gaps (bit 28 between
/// `to_callsign` and `from_callsign`, bit 57 right after
/// `from_callsign`). Per `message.rs::parse_type1_standard` (ground
/// truth, `n29a(29) + n29b(29) + ir(1) + igrid4(15) + i3(3) = 77`), the
/// actual content field is payload bits **58-76 (19 bits)**: `ir` (bit
/// 58) + `igrid4` (bits 59-73) + `i3` (bits 74-76). Bits 56-57 belong to
/// `from_callsign`'s packed value / suffix flag — already covered by the
/// existing AP3/AP4 callsign injection (bit 57's suffix flag is the one
/// exception: AP3/AP4 don't inject it today, but it is constant across
/// every hypothesis for a fixed QSO partner call, so it carries no
/// discriminating content and is correctly excluded here). This struct
/// therefore uses the verified 58-76 / 19-bit range
/// ([`CONTENT_FIELD_START_BIT`], [`CONTENT_FIELD_LEN`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHypothesis {
    /// The canonical FT8 text this hypothesis was encoded from (e.g.
    /// "K1ABC W1AW RR73").
    pub text: String,
    /// Payload bits 58-76 (`ir` + `igrid4` + `i3`), MSB-first / bit-order
    /// matching `content_bits[i]` == payload bit `58 + i`.
    pub content_bits: [bool; CONTENT_FIELD_LEN],
}

/// Pack a standard message's third whitespace-separated token (the
/// `extra` field: empty, a signal report, or a confirmation token) into
/// `(ir, igrid4)`.
///
/// Mirrors `encoder.rs::packgrid`'s token/report branches bit-for-bit
/// (grid-square packing is intentionally omitted: `enumerate_a8_expected
/// _texts` — the only populator of `QsoAp::expected_next_message_texts`
/// today — never produces one). Duplicated here rather than calling
/// `encoder::packgrid` directly because the `encoder` module is gated
/// behind the `transmit` feature and this module (used on every decode,
/// AP or not) is not — the same precedent as the standalone `pack28`
/// above and `ConfirmationToken::igrid4_value` below. Cross-checked
/// against the real encoder in
/// `content_hypotheses_match_real_encoder_ground_truth` (`--features
/// transmit`).
///
/// Returns `None` if `extra` isn't a recognized token/report — the
/// caller treats that as an enumerator bug (log + skip), not a runtime
/// condition to recover from.
fn pack_extra_field(extra: &str) -> Option<(bool, u16)> {
    const MAXGRID4: u16 = 32400;

    if extra.is_empty() {
        return Some((false, MAXGRID4 + 1));
    }
    match extra {
        "RRR" => return Some((false, ConfirmationToken::Rrr.igrid4_value())),
        "RR73" => return Some((false, ConfirmationToken::RR73.igrid4_value())),
        "73" => return Some((false, ConfirmationToken::Final73.igrid4_value())),
        _ => {}
    }

    // Signal report: "R+dd"/"R-dd" (ir=1) or bare "+dd"/"-dd" (ir=0).
    let (ir, report_str) = match extra.strip_prefix('R') {
        Some(rest) if rest.starts_with('+') || rest.starts_with('-') => (true, rest),
        _ => (false, extra),
    };
    let dd: i32 = report_str.parse().ok()?;
    if !(-35..=30).contains(&dd) {
        return None;
    }
    let igrid4 = MAXGRID4 + (35 + dd) as u16;
    Some((ir, igrid4))
}

/// Encode each of `qso.expected_next_message_texts` (via
/// [`pack_extra_field`], the standalone mirror of this crate's canonical
/// `encoder.rs::packgrid` content-packing) and extract the content-field
/// bits (payload bits 58-76 — see [`ContentHypothesis`]'s doc for why
/// this isn't 56-76). Returns hypotheses in the SAME order as the input
/// `Vec<String>` — ordering by SNR-seeded likelihood is the caller's job
/// (Task 3/5), not this pure builder's.
///
/// Returns an empty `Vec` if `qso.expected_next_message_texts` is empty.
/// A text that fails to encode indicates a bug in the enumerator (every
/// `enumerate_a8_expected_texts` output is a well-formed `<to> <from>
/// <extra>` standard message), not a runtime condition to recover from
/// gracefully — log a warning and skip that one hypothesis, don't panic.
pub fn build_content_hypotheses(qso: &QsoAp) -> Vec<ContentHypothesis> {
    let mut out = Vec::with_capacity(qso.expected_next_message_texts.len());

    for text in &qso.expected_next_message_texts {
        let extra = text.split_whitespace().nth(2).unwrap_or("");
        let Some((ir, igrid4)) = pack_extra_field(extra) else {
            tracing::warn!(
                text = %text,
                "build_content_hypotheses: failed to pack content field \
                 (extra = {extra:?}) -- skipping; this indicates a bug in \
                 enumerate_a8_expected_texts, not a runtime condition"
            );
            continue;
        };

        let mut content_bits = [false; CONTENT_FIELD_LEN];
        content_bits[0] = ir;
        content_bits[1..16].copy_from_slice(&u16_to_bits_15(igrid4));
        // i3 = 1 (the "standard message" family every RR73/RRR/73/report
        // reply uses), MSB-first: bits 74,75,76 = false,false,true.
        content_bits[16] = false;
        content_bits[17] = false;
        content_bits[18] = true;

        out.push(ContentHypothesis {
            text: text.clone(),
            content_bits,
        });
    }

    out
}

/// Normalise a decoded message text for matching against the a8
/// expected-templates list. Collapses interior whitespace runs to a
/// single space and uppercases the result.
pub(crate) fn normalize_for_a8_match(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_uppercase()
}

/// Full AP context holding all known information for AP-enhanced decoding.
#[derive(Debug, Clone, Default)]
pub struct ApContext {
    /// Own station callsign.
    pub my_call: Option<MyCallAp>,
    /// Recently heard callsigns (candidates for AP2).
    pub recent_calls: Vec<RecentCallAp>,
    /// Retained for back-compat with existing Ap1-Ap4 call sites that read
    /// a single QSO -- always set to `active_qsos.first().cloned()` by
    /// whichever constructor populates both, so old and new readers agree.
    pub active_qso: Option<QsoAp>,
    /// Ranked (highest priority first), capped at `MAX_AP_QSOS`. Empty
    /// when no QSOs are in flight. The Ap5 hypothesis loop (Task 6)
    /// iterates this; existing Ap3/Ap4 keep reading `active_qso` unchanged.
    pub active_qsos: Vec<QsoAp>,
}

// ---------------------------------------------------------------------------
// LLR injection
// ---------------------------------------------------------------------------

/// Inject a single bit into the LLR array at the given position.
/// `true` bits → negative LLR (bit = 1), `false` → positive LLR (bit = 0).
#[inline]
fn inject_bit(llrs: &mut [f32], pos: usize, bit: bool) {
    if pos < llrs.len() {
        llrs[pos] = if bit {
            -AP_LLR_MAGNITUDE
        } else {
            AP_LLR_MAGNITUDE
        };
    }
}

/// The XOR-whitening bit at payload/codeword position `pos`, for an
/// optional whitening sequence (`None` for FT8/FT2 — no scrambling).
///
/// Mirrors `decoder.rs::par_apply_xor`'s bit addressing exactly: byte
/// index = `pos / 8`, bit within the byte taken MSB-first (`bit_pos = pos
/// % 8`, tested against `0x80 >> bit_pos`). FT4 (`ProtocolParams::ft4()`)
/// XOR-scrambles the 77-bit payload with `FT4_XOR_SEQUENCE` *before* LDPC
/// encoding (`encoder.rs::payload_to_symbols_protocol`) and un-scrambles
/// it the same way after LDPC decode + CRC check
/// (`decoder.rs::par_apply_xor`). AP injection sets LLR signs in that
/// same pre-un-XOR (whitened) codeword-bit space, so an injected "real"
/// payload bit must be whitened with this same sequence before its LLR
/// sign is set — otherwise roughly half the injected positions carry the
/// wrong sign for FT4, actively fighting the correct decode.
#[inline]
fn xor_bit_at(xor_sequence: Option<&[u8; 10]>, pos: usize) -> bool {
    match xor_sequence {
        Some(seq) => {
            let byte_idx = pos / 8;
            let bit_pos = pos % 8;
            byte_idx < seq.len() && (seq[byte_idx] >> (7 - bit_pos)) & 1 == 1
        }
        None => false,
    }
}

/// Inject 28 known bits starting at `offset` in the LLR array.
///
/// `xor_sequence`: `Some(FT4_XOR_SEQUENCE)` when injecting for FT4 (the
/// bits are whitened before their LLR sign is set, matching the
/// pre-un-XOR codeword domain the LDPC decoder operates in); `None` for
/// FT8/FT2 (no whitening, byte-identical to the pre-fix behavior).
fn inject_28_bits(
    llrs: &mut [f32],
    offset: usize,
    bits: &[bool; 28],
    xor_sequence: Option<&[u8; 10]>,
) {
    for (i, &b) in bits.iter().enumerate() {
        let pos = offset + i;
        inject_bit(llrs, pos, b ^ xor_bit_at(xor_sequence, pos));
    }
}

/// Inject AP LLRs according to the given level and context.
///
/// # Arguments
/// * `llrs` - mutable slice of LLR values (must be at least 77 elements for
///   a full FT8 payload, though the function tolerates shorter slices).
/// * `level` - the AP level to apply.
/// * `context` - the AP context containing known callsigns / QSO state.
/// * `xor_sequence` - `Some(FT4_XOR_SEQUENCE)` when decoding FT4 (bits are
///   whitened before their LLR sign is set, matching the pre-un-XOR
///   codeword domain the LDPC decoder operates in — see [`xor_bit_at`]);
///   `None` for FT8/FT2, where injection is byte-identical to before this
///   parameter existed (no whitening applied).
pub fn inject_ap_llrs(
    llrs: &mut [f32],
    level: ApLevel,
    context: &ApContext,
    xor_sequence: Option<&[u8; 10]>,
) {
    match level {
        ApLevel::Ap0 => { /* no injection */ }

        ApLevel::Ap1 => {
            // Inject own callsign at bits 0-27 (to_callsign / called
            // station — we are always the addressee).
            if let Some(ref my_call) = context.my_call {
                inject_28_bits(llrs, 0, &my_call.bits, xor_sequence);
            }
        }

        ApLevel::Ap2 => {
            // AP2 is caller-specific; use inject_ap2_caller() directly.
            // This path is a no-op — the caller chooses which RecentCallAp
            // to inject via inject_ap2_caller().
        }

        ApLevel::Ap3 => {
            // Inject own callsign at bits 0-27 (to_callsign / called station)
            if let Some(ref my_call) = context.my_call {
                inject_28_bits(llrs, 0, &my_call.bits, xor_sequence);
            }
            // Inject active QSO partner at bits 29-56 (from_callsign /
            // calling station)
            if let Some(ref qso) = context.active_qso {
                inject_28_bits(llrs, 29, &qso.their_bits, xor_sequence);
            }
        }

        ApLevel::Ap4 => {
            // Same as AP3 …
            if let Some(ref my_call) = context.my_call {
                inject_28_bits(llrs, 0, &my_call.bits, xor_sequence);
            }
            if let Some(ref qso) = context.active_qso {
                inject_28_bits(llrs, 29, &qso.their_bits, xor_sequence);
            }
            // … plus i3 type bits at 74-76 = false, false, true (i3=1,
            // the "standard message" family RR73/RRR/73 actually use —
            // verified empirically against this project's own encoder,
            // see pancetta-ft8/tests/ap_i3_tests.rs. i3=0 selects the
            // FreeText/Telemetry/contest family, which
            // Ft8Message::is_plausible() unconditionally rejects, so the
            // previous (0,0,0) injection made AP4 fight the very message
            // class it exists to help decode. Whitened for FT4 like the
            // callsign fields above — the i3 field is part of the same
            // 77-bit payload the XOR scrambles.
            inject_bit(llrs, 74, false ^ xor_bit_at(xor_sequence, 74));
            inject_bit(llrs, 75, false ^ xor_bit_at(xor_sequence, 75));
            inject_bit(llrs, 76, true ^ xor_bit_at(xor_sequence, 76));
        }

        ApLevel::Ap5(hyp) => {
            // Same callsign injection as Ap3: own call at bits 0-27
            // (to_callsign), active QSO partner at bits 29-56
            // (from_callsign).
            if let Some(ref my_call) = context.my_call {
                inject_28_bits(llrs, 0, &my_call.bits, xor_sequence);
            }
            if let Some(ref qso) = context.active_qso {
                inject_28_bits(llrs, 29, &qso.their_bits, xor_sequence);
            }
            // Plus the enumerated content hypothesis's bits at payload
            // positions 58-76 (`ir` + `igrid4` + `i3`) -- the specific
            // completion content this attempt assumes.
            for (i, &b) in hyp.content_bits.iter().enumerate() {
                let pos = CONTENT_FIELD_START_BIT + i;
                inject_bit(llrs, pos, b ^ xor_bit_at(xor_sequence, pos));
            }
        }

        ApLevel::Cq => {
            // Context-free: "CQ" is a fixed protocol token, not a personal
            // callsign, so `context` is intentionally unused here.
            if let Some((cq_packed, cq_ip)) = pack28("CQ") {
                let cq_bits = u32_to_bits_28(cq_packed);
                inject_28_bits(llrs, 0, &cq_bits, xor_sequence);
                // Bit 28: to_callsign suffix flag. pack28("CQ") always
                // returns ip=0 (no /P or /R on a bare "CQ" token).
                inject_bit(llrs, 28, (cq_ip != 0) ^ xor_bit_at(xor_sequence, 28));
            }
            // i3=1 ("standard message" family) — same assumption AP4
            // makes, and the only i3 value this project's own encoder
            // ever emits for standard-message text (see
            // `pancetta-ft8/tests/ap_i3_tests.rs`).
            inject_bit(llrs, 74, false ^ xor_bit_at(xor_sequence, 74));
            inject_bit(llrs, 75, false ^ xor_bit_at(xor_sequence, 75));
            inject_bit(llrs, 76, true ^ xor_bit_at(xor_sequence, 76));
        }
    }
}

/// Inject the full message-content mask for a QSO-confirmation token
/// (RRR/RR73/73), on top of the existing AP4 callsign + i3 injection
/// (call [`inject_ap_llrs`] with `ApLevel::Ap4` first, then this): the
/// `ir` bit (payload bit 58, always 0 for these three tokens — none of
/// them carry an R-prefix) and the full 15-bit `igrid4` field (bits
/// 59-73) for the assumed token. AP4 alone only constrains the message
/// TYPE (i3=1); this additionally constrains the specific completion
/// CONTENT.
///
/// `xor_sequence`: see [`inject_ap_llrs`].
pub fn inject_confirmation_token_bits(
    llrs: &mut [f32],
    token: ConfirmationToken,
    xor_sequence: Option<&[u8; 10]>,
) {
    inject_bit(llrs, 58, false ^ xor_bit_at(xor_sequence, 58));
    let bits15 = u16_to_bits_15(token.igrid4_value());
    for (i, &b) in bits15.iter().enumerate() {
        let pos = 59 + i;
        inject_bit(llrs, pos, b ^ xor_bit_at(xor_sequence, pos));
    }
}

/// Inject a specific recent callsign at bits 29-56 (AP2 calling station /
/// `from_callsign`).
///
/// This is called externally for each candidate caller when attempting AP2
/// decoding passes. `xor_sequence`: see [`inject_ap_llrs`].
pub fn inject_ap2_caller(llrs: &mut [f32], caller: &RecentCallAp, xor_sequence: Option<&[u8; 10]>) {
    inject_28_bits(llrs, 29, &caller.bits, xor_sequence);
}

/// Inject a specific recent callsign at bits 0-27 (called station /
/// `to_callsign`).
///
/// Companion to `inject_ap2_caller`. Used by hb-043 my_call-less AP
/// injection — when the operator is scanning rather than transmitting,
/// observed callsigns are still useful priors but might appear at EITHER
/// position. This function handles the called-position injection.
/// `xor_sequence`: see [`inject_ap_llrs`].
pub fn inject_recent_call_at_called(
    llrs: &mut [f32],
    call: &RecentCallAp,
    xor_sequence: Option<&[u8; 10]>,
) {
    inject_28_bits(llrs, 0, &call.bits, xor_sequence);
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_u32_to_bits_28() {
        // All zeros
        assert_eq!(u32_to_bits_28(0), [false; 28]);

        // MSB only
        let bits = u32_to_bits_28(1 << 27);
        assert!(bits[0]);
        assert!(!bits[1]);
        assert!(!bits[27]);

        // LSB only
        let bits = u32_to_bits_28(1);
        assert!(!bits[0]);
        assert!(bits[27]);

        // All ones (28-bit)
        let bits = u32_to_bits_28(0x0FFF_FFFF);
        assert!(bits.iter().all(|&b| b));
    }

    #[test]
    fn test_my_call_ap_creation() {
        let ap = MyCallAp::new("K1ABC").expect("K1ABC should encode");
        assert_eq!(ap.callsign, "K1ABC");
        // Verify round-trip: bits should reconstruct the packed value
        let mut reconstructed: u32 = 0;
        for (i, &b) in ap.bits.iter().enumerate() {
            if b {
                reconstructed |= 1 << (27 - i);
            }
        }
        assert_eq!(reconstructed, ap.packed_28);

        // Invalid callsign should return None
        assert!(MyCallAp::new("!!!").is_none());
    }

    #[test]
    fn test_inject_ap1() {
        let my_call = MyCallAp::new("K1ABC").expect("K1ABC should encode");
        let ctx = ApContext {
            my_call: Some(my_call.clone()),
            recent_calls: vec![],
            active_qso: None,
            active_qsos: vec![],
        };

        let mut llrs = vec![0.0f32; 77];
        inject_ap_llrs(&mut llrs, ApLevel::Ap1, &ctx, None);

        // Bits 0-27 should be injected with own call (to_callsign / called
        // station — we are always the addressee for any message we decode).
        for i in 0..28 {
            let expected_bit = my_call.bits[i];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} mismatch", i);
        }

        // Bits 28-76 should be untouched
        for i in 28..77 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }
    }

    #[test]
    fn test_inject_ap3() {
        let my_call = MyCallAp::new("K1ABC").expect("K1ABC should encode");
        let qso = QsoAp::new("W1AW", QsoApProgress::WaitingForReport).expect("W1AW should encode");
        let ctx = ApContext {
            my_call: Some(my_call.clone()),
            recent_calls: vec![],
            active_qso: Some(qso.clone()),
            active_qsos: vec![],
        };

        let mut llrs = vec![0.0f32; 77];
        inject_ap_llrs(&mut llrs, ApLevel::Ap3, &ctx, None);

        // Bits 0-27: my callsign (K1ABC) — to_callsign / called station.
        // We are always the addressee for any message we decode.
        for i in 0..28 {
            let expected_bit = my_call.bits[i];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} (my call) mismatch", i);
        }

        // Bit 28 (to_callsign suffix flag) untouched — not part of either
        // 28-bit callsign field.
        assert_eq!(
            llrs[28], 0.0,
            "bit 28 (suffix flag gap) should be untouched"
        );

        // Bits 29-56: their callsign (W1AW) — from_callsign / calling
        // station.
        for i in 29..57 {
            let expected_bit = qso.their_bits[i - 29];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} (their call) mismatch", i);
        }

        // Bits 57-76 should be untouched
        for i in 57..77 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }
    }

    #[test]
    fn ap5_injects_callsigns_and_content_bits() {
        let my_call = MyCallAp::new("K1ABC").expect("K1ABC should encode");
        let mut qso =
            QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).expect("W1AW should encode");
        qso = qso.with_expected_texts(["K1ABC W1AW RR73"]);
        let hyps = build_content_hypotheses(&qso);
        assert_eq!(hyps.len(), 1, "RR73 must produce exactly one hypothesis");
        let hyp = hyps[0].clone();
        assert_eq!(hyp.text, "K1ABC W1AW RR73");

        let ctx = ApContext {
            my_call: Some(my_call.clone()),
            recent_calls: vec![],
            active_qso: Some(qso.clone()),
            active_qsos: vec![],
        };

        let mut llrs = vec![0.0f32; 77];
        inject_ap_llrs(&mut llrs, ApLevel::Ap5(hyp.clone()), &ctx, None);

        // Bits 0-27: my callsign (K1ABC) -- to_callsign / called station,
        // same convention as Ap3's own test.
        for i in 0..28 {
            let expected_bit = my_call.bits[i];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} (my call) mismatch", i);
        }

        // Bit 28 (to_callsign suffix flag gap) untouched.
        assert_eq!(
            llrs[28], 0.0,
            "bit 28 (suffix flag gap) should be untouched"
        );

        // Bits 29-56: their callsign (W1AW) -- from_callsign / calling
        // station.
        for i in 29..57 {
            let expected_bit = qso.their_bits[i - 29];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} (their call) mismatch", i);
        }

        // Bit 57 (gap before the content field at bit 58) untouched.
        assert_eq!(llrs[57], 0.0, "bit 57 (gap) should be untouched");

        // Bits 58-76: the content hypothesis's bits, same sign convention
        // (real values from `build_content_hypotheses`, ground-truthed
        // against the real encoder in Task 1 -- not a placeholder).
        for i in 0..CONTENT_FIELD_LEN {
            let expected_bit = hyp.content_bits[i];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(
                llrs[CONTENT_FIELD_START_BIT + i],
                expected_llr,
                "content bit {} mismatch",
                i
            );
        }
    }

    #[test]
    fn test_inject_ap2_caller_uses_from_callsign_offset() {
        // AP2's candidate-caller injection targets bits 29-56
        // (from_callsign / calling station) — the other station, since we
        // are always the addressee.
        let caller = RecentCallAp::new("W1AW", -10.0).expect("W1AW should encode");
        let mut llrs = vec![0.0f32; 77];
        inject_ap2_caller(&mut llrs, &caller, None);

        for i in 0..29 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }
        for i in 29..57 {
            let expected_bit = caller.bits[i - 29];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} mismatch", i);
        }
        for i in 57..77 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }
    }

    #[test]
    fn test_inject_recent_call_at_called_uses_to_callsign_offset() {
        // The companion "called" injection targets bits 0-27
        // (to_callsign) — as if the recent call were the addressee.
        let recent = RecentCallAp::new("K1ABC", -5.0).expect("K1ABC should encode");
        let mut llrs = vec![0.0f32; 77];
        inject_recent_call_at_called(&mut llrs, &recent, None);

        for i in 0..28 {
            let expected_bit = recent.bits[i];
            let expected_llr = if expected_bit {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} mismatch", i);
        }
        for i in 28..77 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }
    }

    #[test]
    fn test_inject_cq_uses_pack28_cq_token_at_called_position() {
        // "CQ" special token per pack28 is packed value 2 (see the `match
        // callsign { "CQ" => return Some((2, 0)), ... }` arm above).
        let (cq_packed, cq_ip) = pack28("CQ").expect("CQ must encode");
        assert_eq!(cq_packed, 2);
        assert_eq!(cq_ip, 0);

        let ctx = ApContext::default();
        let mut llrs = vec![0.0f32; 77];
        inject_ap_llrs(&mut llrs, ApLevel::Cq, &ctx, None);

        let expected_bits = u32_to_bits_28(cq_packed);
        for i in 0..28 {
            let expected_llr = if expected_bits[i] {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(llrs[i], expected_llr, "bit {} (CQ token) mismatch", i);
        }
        // Bit 28 (suffix flag): CQ's ip=0 -> false -> positive LLR.
        assert_eq!(llrs[28], AP_LLR_MAGNITUDE, "bit 28 (suffix flag) mismatch");
        // i3 bits 74-76 = (0,0,1) — same "standard message" assumption AP4
        // makes.
        assert_eq!(llrs[74], AP_LLR_MAGNITUDE);
        assert_eq!(llrs[75], AP_LLR_MAGNITUDE);
        assert_eq!(llrs[76], -AP_LLR_MAGNITUDE);
        // Bits 29-73 (from_callsign + ir + igrid4) untouched — CQ mask
        // doesn't know or constrain who's calling or what grid they sent.
        for i in 29..74 {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched by CQ mask", i);
        }
    }

    #[test]
    fn test_inject_cq_requires_no_context() {
        // The whole point of ApLevel::Cq: it must inject identically
        // regardless of what's in the ApContext (my_call/active_qso
        // present or absent) — "CQ" is a fixed protocol token, not a
        // personal callsign.
        let empty_ctx = ApContext::default();
        let full_ctx = ApContext {
            my_call: Some(MyCallAp::new("K1ABC").unwrap()),
            recent_calls: vec![RecentCallAp::new("W1AW", -5.0).unwrap()],
            active_qso: Some(QsoAp::new("W1AW", QsoApProgress::WaitingForReport).unwrap()),
            active_qsos: vec![],
        };

        let mut llrs_empty = vec![0.0f32; 77];
        inject_ap_llrs(&mut llrs_empty, ApLevel::Cq, &empty_ctx, None);
        let mut llrs_full = vec![0.0f32; 77];
        inject_ap_llrs(&mut llrs_full, ApLevel::Cq, &full_ctx, None);

        assert_eq!(
            llrs_empty, llrs_full,
            "ApLevel::Cq injection must be identical regardless of ApContext contents"
        );
    }

    #[test]
    fn active_qso_stays_in_sync_with_active_qsos_first() {
        let qso1 = QsoAp::new("K1ABC", QsoApProgress::WaitingForReport).unwrap();
        let qso2 = QsoAp::new("K2DEF", QsoApProgress::WaitingForReport).unwrap();
        let ctx = ApContext {
            my_call: None,
            recent_calls: vec![],
            active_qso: Some(qso1.clone()),
            active_qsos: vec![qso1.clone(), qso2],
        };
        assert_eq!(
            ctx.active_qso.as_ref().unwrap().their_call,
            ctx.active_qsos[0].their_call
        );
    }

    #[test]
    fn test_inject_confirmation_token_bits_rr73() {
        // MAXGRID4 = 32400; RR73 = +3 = 32403.
        const MAXGRID4: u16 = 32400;
        let mut llrs = vec![0.0f32; 77];
        inject_confirmation_token_bits(&mut llrs, ConfirmationToken::RR73, None);

        // ir bit (58) = 0 -> positive LLR.
        assert_eq!(llrs[58], AP_LLR_MAGNITUDE, "ir bit must be 0 (no R-prefix)");

        let expected_bits = u16_to_bits_15(MAXGRID4 + 3);
        for i in 0..15 {
            let expected_llr = if expected_bits[i] {
                -AP_LLR_MAGNITUDE
            } else {
                AP_LLR_MAGNITUDE
            };
            assert_eq!(
                llrs[59 + i],
                expected_llr,
                "igrid4 bit {} (payload bit {}) mismatch",
                i,
                59 + i
            );
        }
        // Everything else untouched.
        for i in (0..58).chain(74..77) {
            assert_eq!(llrs[i], 0.0, "bit {} should be untouched", i);
        }
    }

    #[test]
    fn test_confirmation_token_igrid4_values_match_maxgrid4_offsets() {
        const MAXGRID4: u16 = 32400;
        assert_eq!(ConfirmationToken::Rrr.igrid4_value(), MAXGRID4 + 2);
        assert_eq!(ConfirmationToken::RR73.igrid4_value(), MAXGRID4 + 3);
        assert_eq!(ConfirmationToken::Final73.igrid4_value(), MAXGRID4 + 4);
    }

    // -----------------------------------------------------------------
    // build_content_hypotheses
    // -----------------------------------------------------------------

    #[test]
    fn build_content_hypotheses_extracts_bits_58_to_76() {
        let mut qso = QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).unwrap();
        qso = qso.with_expected_texts(["K1ABC W1AW RR73", "K1ABC W1AW RRR", "K1ABC W1AW 73"]);
        let hyps = build_content_hypotheses(&qso);
        assert_eq!(hyps.len(), 3);
        assert_eq!(hyps[0].text, "K1ABC W1AW RR73");

        // Real values, derived from ConfirmationToken::igrid4_value (already
        // verified against this project's own encoder in
        // test_confirmation_token_igrid4_values_match_maxgrid4_offsets /
        // ap_i3_tests.rs): ir=0 for all three tokens (no R-prefix), i3=1
        // (standard message family) for all three.
        const MAXGRID4: u16 = 32400;
        let expected = |igrid4: u16| -> [bool; CONTENT_FIELD_LEN] {
            let mut bits = [false; CONTENT_FIELD_LEN];
            bits[1..16].copy_from_slice(&u16_to_bits_15(igrid4));
            bits[18] = true; // i3 = 1
            bits
        };
        assert_eq!(hyps[0].content_bits, expected(MAXGRID4 + 3), "RR73");
        assert_eq!(hyps[1].content_bits, expected(MAXGRID4 + 2), "RRR");
        assert_eq!(hyps[2].content_bits, expected(MAXGRID4 + 4), "73");

        // The three confirmation hypotheses must produce three DIFFERENT
        // content-bit patterns (if they collided, Ap5 could never
        // distinguish them) -- the load-bearing property.
        assert_ne!(hyps[0].content_bits, hyps[1].content_bits);
        assert_ne!(hyps[1].content_bits, hyps[2].content_bits);
        assert_ne!(hyps[0].content_bits, hyps[2].content_bits);
    }

    #[test]
    fn build_content_hypotheses_empty_when_no_expected_texts() {
        let qso = QsoAp::new("W1AW", QsoApProgress::WaitingForReport).unwrap();
        assert!(build_content_hypotheses(&qso).is_empty());
    }

    #[test]
    fn build_content_hypotheses_distinguishes_report_values() {
        // WaitingForReport hypotheses (R-12, -12, R-10, ...) must also
        // produce pairwise-distinct content bits -- this is the numeric
        // (igrid4 = MAXGRID4 + 35 + dd) path, not the fixed-token path
        // exercised above.
        let mut qso = QsoAp::new("W1AW", QsoApProgress::WaitingForReport).unwrap();
        qso = qso.with_expected_texts(["K1ABC W1AW R-12", "K1ABC W1AW -12", "K1ABC W1AW R-10"]);
        let hyps = build_content_hypotheses(&qso);
        assert_eq!(hyps.len(), 3);
        // R-12 vs -12: same igrid4, but the ir bit differs.
        assert_ne!(hyps[0].content_bits, hyps[1].content_bits);
        assert!(hyps[0].content_bits[0], "R-12 must set ir=1");
        assert!(!hyps[1].content_bits[0], "-12 (no R) must set ir=0");
        // R-12 vs R-10: different report value -> different igrid4.
        assert_ne!(hyps[0].content_bits, hyps[2].content_bits);
    }

    #[test]
    fn build_content_hypotheses_skips_unencodable_extra_and_warns() {
        // Not produced by enumerate_a8_expected_texts, but with_expected_texts
        // accepts arbitrary caller text -- an unencodable `extra` token must
        // be skipped, not panic.
        let mut qso = QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).unwrap();
        qso = qso.with_expected_texts(["K1ABC W1AW GARBAGE123", "K1ABC W1AW RR73"]);
        let hyps = build_content_hypotheses(&qso);
        assert_eq!(
            hyps.len(),
            1,
            "the unencodable hypothesis must be skipped, not panic"
        );
        assert_eq!(hyps[0].text, "K1ABC W1AW RR73");
    }

    /// Ground-truth cross-check: `pack_extra_field`'s standalone mirror of
    /// `encoder.rs::packgrid` must match the REAL encoder byte-for-byte at
    /// payload bits 58-76, for both the fixed-token and numeric-report
    /// paths. Guards against `pack_extra_field` silently drifting from the
    /// canonical encode path it duplicates (see its doc comment for why it
    /// can't just call `encoder::packgrid` -- the `transmit` feature gate).
    #[cfg(feature = "transmit")]
    #[test]
    fn content_hypotheses_match_real_encoder_ground_truth() {
        use crate::ldpc::gray_to_binary;
        use crate::message::PAYLOAD_BITS;
        use crate::{Ft8Encoder, NUM_SYMBOLS};

        fn encode_to_payload_bits_58_76(text: &str) -> [bool; CONTENT_FIELD_LEN] {
            let mut encoder = Ft8Encoder::new();
            let symbols = encoder
                .encode_message(text, None)
                .unwrap_or_else(|e| panic!("failed to encode {text:?}: {e}"));

            let mut codeword_bits = Vec::with_capacity(174);
            for i_tone in 0..NUM_SYMBOLS {
                let is_data = (7..36).contains(&i_tone) || (43..72).contains(&i_tone);
                if !is_data {
                    continue;
                }
                let v = gray_to_binary(symbols[i_tone]);
                codeword_bits.push((v & 4) != 0);
                codeword_bits.push((v & 2) != 0);
                codeword_bits.push((v & 1) != 0);
            }
            let payload = &codeword_bits[0..PAYLOAD_BITS];

            let mut bits = [false; CONTENT_FIELD_LEN];
            bits.copy_from_slice(
                &payload[CONTENT_FIELD_START_BIT..CONTENT_FIELD_START_BIT + CONTENT_FIELD_LEN],
            );
            bits
        }

        for text in [
            "K1ABC W1AW RR73",
            "K1ABC W1AW RRR",
            "K1ABC W1AW 73",
            "K1ABC W1AW R-12",
            "K1ABC W1AW -12",
            "K1ABC W1AW R+05",
        ] {
            let mut qso = QsoAp::new("W1AW", QsoApProgress::WaitingForConfirmation).unwrap();
            qso = qso.with_expected_texts([text]);
            let hyps = build_content_hypotheses(&qso);
            assert_eq!(hyps.len(), 1, "{text} should encode");
            assert_eq!(
                hyps[0].content_bits,
                encode_to_payload_bits_58_76(text),
                "build_content_hypotheses's content bits for {text:?} must match \
                 this project's own real encoder at payload bits 58-76"
            );
        }
    }
}
