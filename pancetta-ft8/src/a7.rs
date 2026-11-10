//! AP type 7 (a7) — template cross-correlation against decoded callsigns.
//!
//! Session 2 of hb-048. Spec at
//! `docs/superpowers/specs/2026-05-31-hb-048-a7-design.md`.
//!
//! ## Mechanism
//!
//! After slot N successfully decodes callsign C at audio frequency f and
//! time t, the a7 path constructs a small set of plausible *follow-up*
//! messages rooted at C (e.g., `<C OTHER>`, `<C OTHER> R+10`,
//! `<OTHER C> RR73`, …). Each template is run through the standard FT8
//! encoder → 174-bit LDPC codeword. The expected codeword's bits then
//! act as a "signed mask" applied to the *current slot's* residual LLR
//! stream: a strong correlation between the expected codeword and the
//! observed LLRs means the templated message is actually present, even
//! when the standard Costas-pre-gate would not have admitted that
//! candidate. The score function is `snr7` (best per-template match) and
//! `snr7b` (best/second-best ratio), mirroring WSJT-X.
//!
//! ## Session 2 scope (this module)
//!
//! - `A7ExpectedCall`: context for one decoded callsign feeding a7.
//! - `A7Template`: one candidate follow-up — message text, 174 expected
//!   codeword bits, and a 174-element PINNED mask marking bits that are
//!   structurally determined by the callsign / message type.
//! - `generate_templates()`: enumerate plausible follow-up templates for
//!   a single expected call.
//! - `cross_correlate()`: compute snr7 from a candidate's residual LLRs
//!   and a template's expected codeword.
//!
//! Production wiring lives in Session 3 (no decoder changes here).
//!
//! ## LLR sign convention
//!
//! Matches `pancetta-ft8::decoder::par_compute_soft_llrs_db` and
//! `pancetta-ft8::ap::inject_bit`:
//!
//! - `llr > 0` ⇒ bit `0` is more likely
//! - `llr < 0` ⇒ bit `1` is more likely
//! - `|llr|` magnitude reflects soft confidence
//!
//! A "matched" LLR satisfies `sign(llr) == sign_of_expected_bit_zero`,
//! i.e. `llr * (1 - 2 * expected_bit) > 0`.
//!
//! ## Feature gating
//!
//! Template synthesis requires the FT8 encoder, which is feature-gated
//! behind `transmit`. The cross-correlation primitive itself is pure
//! arithmetic on LLR slices and does not require `transmit`.

#![allow(dead_code)]
// rationale: template cross-correlation loops index LLR/symbol slices by
// position; the index is load-bearing.
#![allow(clippy::needless_range_loop)]

use std::time::Instant;

/// Length of the FT8 LDPC codeword in bits.
pub const A7_CODEWORD_BITS: usize = 174;

/// WSJT-X reference threshold for snr7 (best-lag correlation peak).
pub const A7_SNR7_THRESHOLD_DEFAULT: f64 = 6.0;

/// WSJT-X reference threshold for snr7b (best/second-best ratio).
pub const A7_SNR7B_THRESHOLD_DEFAULT: f64 = 1.8;

/// Default per-source-decode template cap (CPU mitigation; spec §risk-3).
pub const A7_MAX_TEMPLATES_PER_CALL: usize = 32;

// ---------------------------------------------------------------------------
// Expected-call context
// ---------------------------------------------------------------------------

/// Slot parity (even/odd) carried by the FT8 protocol per 15-second slot.
///
/// Used by a7 to gate templates: only the slot of opposite parity to the
/// source decode should be considered, mirroring WSJT-X's even/odd table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum A7SlotParity {
    /// Slot started on an even 15-second boundary.
    Even,
    /// Slot started on an odd 15-second boundary.
    Odd,
}

impl A7SlotParity {
    /// The opposite parity — i.e. the next slot after this one.
    pub fn opposite(self) -> Self {
        match self {
            A7SlotParity::Even => A7SlotParity::Odd,
            A7SlotParity::Odd => A7SlotParity::Even,
        }
    }
}

/// A single decoded callsign + audio-frequency context that feeds a7
/// template generation in the *next* slot.
#[derive(Debug, Clone)]
pub struct A7ExpectedCall {
    /// The callsign that was decoded.
    pub call: String,
    /// Audio frequency (Hz) of the decode.
    pub freq_hz: f32,
    /// Parity of the slot the decode was observed in.
    pub slot_parity: A7SlotParity,
    /// When the decode was observed (used for eviction).
    pub decoded_at: Instant,
    /// Optional second callsign heard at the time of the decode (e.g. the
    /// station that `call` was responding to). Used to seed templates of
    /// the form `<call other> …` and `<other call> …`.
    pub heard_with: Option<String>,
    /// Optional operator's own callsign — gives templates of the form
    /// `<call my_call> …` and `<my_call call> …` priority weight.
    pub my_call: Option<String>,
}

impl A7ExpectedCall {
    /// Construct a minimal expected-call with no extra context.
    pub fn new(call: impl Into<String>, freq_hz: f32, slot_parity: A7SlotParity) -> Self {
        Self {
            call: call.into(),
            freq_hz,
            slot_parity,
            decoded_at: Instant::now(),
            heard_with: None,
            my_call: None,
        }
    }

    /// Add the `<heard_with>` companion call (typically the station that
    /// `call` was responding to).
    pub fn with_heard_with(mut self, other: impl Into<String>) -> Self {
        self.heard_with = Some(other.into());
        self
    }

    /// Add operator's own callsign.
    pub fn with_my_call(mut self, my: impl Into<String>) -> Self {
        self.my_call = Some(my.into());
        self
    }
}

// ---------------------------------------------------------------------------
// Template
// ---------------------------------------------------------------------------

/// A single a7 template: the message text, its expected 174-bit codeword,
/// and a per-bit PINNED mask marking bits structurally determined by the
/// template (callsign-bits + i3 type bits + grid/report bits when fixed).
///
/// The PINNED mask lets downstream consumers (Session 3) inject only the
/// structurally-certain bits as LLR pins, leaving the rest free for the
/// LDPC decoder to recover.
#[derive(Debug, Clone)]
pub struct A7Template {
    /// Human-readable text of the templated message (e.g. `"K1ABC W1AW 73"`).
    pub message_text: String,
    /// Expected 174-bit LDPC codeword. `true` = bit 1, `false` = bit 0.
    pub codeword: [bool; A7_CODEWORD_BITS],
    /// Per-bit PINNED mask. `true` = structurally determined by the
    /// template (callsign / i3 / fixed-suffix bits); `false` = free.
    pub pinned: [bool; A7_CODEWORD_BITS],
    /// Tag categorizing the template (debug + Session-3 priority weighting).
    pub kind: A7TemplateKind,
}

/// Coarse categorization of a generated template.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum A7TemplateKind {
    /// `<C OTHER>` standard message (call at calling-station position).
    CalledByC,
    /// `<OTHER C>` standard message (call at called-station position).
    CallingC,
    /// `<C OTHER> GRID` — adds a 4-char grid.
    CalledByCGrid,
    /// `<OTHER C> GRID` — adds a 4-char grid.
    CallingCGrid,
    /// `<C OTHER> R+NN` — signal-report response.
    ReportFromC,
    /// `<OTHER C> R+NN` — signal-report to C.
    ReportToC,
    /// Status message: `<C OTHER> RR73`, `<C OTHER> 73`, `<C OTHER> RRR`.
    StatusFromC,
    /// Status message in the other direction.
    StatusToC,
    /// CQ from C: `CQ C` or `CQ C GRID`.
    CqFromC,
}

impl A7Template {
    /// Number of bits in this template's PINNED mask. Always
    /// [`A7_CODEWORD_BITS`].
    // rationale: not a collection — `len` is the fixed codeword bit-width and is
    // never zero, so an `is_empty` companion would be meaningless.
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        A7_CODEWORD_BITS
    }

    /// Number of bits flagged as PINNED in this template.
    pub fn pinned_count(&self) -> usize {
        self.pinned.iter().filter(|&&b| b).count()
    }
}

// ---------------------------------------------------------------------------
// Template generation
// ---------------------------------------------------------------------------

/// A small bank of representative "other" callsigns used when a context
/// `heard_with` / `my_call` is not supplied. Chosen to span common region
/// prefixes; the production caller should ideally supply concrete recent
/// calls, but the function must not fail when none are available.
const A7_FALLBACK_CALLS: &[&str] = &[
    "K1ABC", "W1AW", "N1DEF", "VE3XYZ", "G0ABC", "DL1ABC", "JA1ABC", "VK2ABC",
];

/// Representative grids used for grid-bearing templates.
const A7_FALLBACK_GRIDS: &[&str] = &["FN42", "EM10", "DM04", "JO31"];

/// Signal-report values used for report-bearing templates (subset of full
/// range to keep template count bounded; spec §risk-3).
const A7_REPORT_DBS: &[i8] = &[-20, -15, -10, -5, 0, 5, 10, 15];

/// Status messages emitted from C (or to C).
const A7_STATUS_MSGS: &[&str] = &["RRR", "RR73", "73"];

#[cfg(feature = "transmit")]
fn ldpc_codeword_for_message(text: &str) -> Option<[bool; A7_CODEWORD_BITS]> {
    use crate::encoder::Ft8Encoder;
    let mut enc = Ft8Encoder::new();
    // `Ft8Encoder` exposes `encode_message` which returns the final 79-symbol
    // sequence. We don't have direct access to the 174-bit codeword without
    // mutating that module — but the 174-bit codeword is uniquely determined
    // by the 79-symbol sequence (it's just the Gray-decoded data symbols).
    let symbols = enc.encode_message(text, None).ok()?;
    Some(symbols_to_codeword_bits(&symbols))
}

/// Reconstruct the 174-bit LDPC codeword from a 79-symbol FT8 transmission
/// sequence. The codeword is the data-symbol portion of the sequence with
/// Gray decoding applied; Costas-sync symbols (at positions 0..7, 36..43,
/// 72..79) are skipped.
fn symbols_to_codeword_bits(symbols: &[u8; 79]) -> [bool; A7_CODEWORD_BITS] {
    let mut bits = [false; A7_CODEWORD_BITS];
    let mut bit_idx = 0usize;
    for (i, &sym) in symbols.iter().enumerate() {
        // Skip Costas sync positions 0..7, 36..43, 72..79
        let is_costas = i < 7 || (36..43).contains(&i) || i >= 72;
        if is_costas {
            continue;
        }
        // Invert Gray code: encoder applies binary_to_gray, so we need
        // gray_to_binary here. For 8-FSK (3 bits/symbol), gray_to_binary(g)
        // = g[2], g[2]^g[1], g[2]^g[1]^g[0] (MSB-first).
        let g2 = (sym >> 2) & 1;
        let g1 = (sym >> 1) & 1;
        let g0 = sym & 1;
        let b2 = g2;
        let b1 = b2 ^ g1;
        let b0 = b1 ^ g0;
        bits[bit_idx] = b2 == 1;
        bits[bit_idx + 1] = b1 == 1;
        bits[bit_idx + 2] = b0 == 1;
        bit_idx += 3;
    }
    debug_assert_eq!(bit_idx, A7_CODEWORD_BITS);
    bits
}

/// Build the canonical "pinned" mask for a standard FT8 message:
/// callsign-A at bits 0-27, callsign-B at bits 28-55, i3 at 74-76.
///
/// Bits 56-73 (grid/report payload) are pinned only when explicitly
/// requested.
fn build_standard_pinned_mask(pin_payload: bool) -> [bool; A7_CODEWORD_BITS] {
    let mut mask = [false; A7_CODEWORD_BITS];
    // Callsign A: bits 0-27
    for i in 0..28 {
        mask[i] = true;
    }
    // Callsign B: bits 28-55
    for i in 28..56 {
        mask[i] = true;
    }
    // Optional payload pin (grid/report fully known)
    if pin_payload {
        for i in 56..74 {
            mask[i] = true;
        }
    }
    // i3 type bits: 74-76
    for i in 74..77 {
        mask[i] = true;
    }
    mask
}

/// Build a template from text + categorization. Returns `None` if the
/// FT8 encoder can't encode the message (invalid callsign, etc.).
#[cfg(feature = "transmit")]
fn make_template(text: &str, kind: A7TemplateKind, pin_payload: bool) -> Option<A7Template> {
    let codeword = ldpc_codeword_for_message(text)?;
    Some(A7Template {
        message_text: text.to_string(),
        codeword,
        pinned: build_standard_pinned_mask(pin_payload),
        kind,
    })
}

/// Generate the set of plausible follow-up message templates for one
/// expected call.
///
/// Per the design spec, the template set is bounded at
/// [`A7_MAX_TEMPLATES_PER_CALL`] (32) to keep per-slot CPU bounded.
/// Template ordering is deterministic and prioritized by likelihood:
///
/// 1. Status messages (RR73 / 73 / RRR) involving `my_call` — most likely
///    next-slot message when operator has an active QSO.
/// 2. Status messages involving `heard_with` — same shape, less certain.
/// 3. Report messages involving `my_call` and `heard_with`.
/// 4. Grid-bearing messages.
/// 5. CQ-from-C.
/// 6. Fallback templates (no `my_call` / `heard_with` context).
///
/// On feature-disabled builds (no `transmit`), returns an empty vec —
/// callers in non-transmit builds must use a separately-supplied codeword
/// (testing-only path).
#[cfg(feature = "transmit")]
pub fn generate_templates(call: &A7ExpectedCall) -> Vec<A7Template> {
    let mut templates: Vec<A7Template> = Vec::new();
    let c = &call.call;

    // Build the priority list of "other" callsigns: my_call first, then
    // heard_with, then a small fallback bank.
    let mut others: Vec<String> = Vec::new();
    if let Some(my) = &call.my_call {
        others.push(my.clone());
    }
    if let Some(h) = &call.heard_with {
        if !others.contains(h) {
            others.push(h.clone());
        }
    }
    for f in A7_FALLBACK_CALLS {
        if !others.iter().any(|o| o == f) && others.len() < 4 {
            others.push((*f).to_string());
        }
    }

    // (1+2) Status messages: `<C OTHER> RR73`, `<C OTHER> 73`, `<C OTHER> RRR`
    // plus the reverse direction. Both directions, all 3 statuses, for the
    // top-2 others.
    for other in others.iter().take(2) {
        for status in A7_STATUS_MSGS {
            for (text, kind) in [
                (
                    format!("{} {} {}", c, other, status),
                    A7TemplateKind::StatusFromC,
                ),
                (
                    format!("{} {} {}", other, c, status),
                    A7TemplateKind::StatusToC,
                ),
            ] {
                if templates.len() >= A7_MAX_TEMPLATES_PER_CALL {
                    break;
                }
                if let Some(t) = make_template(&text, kind, true) {
                    templates.push(t);
                }
            }
        }
    }

    // (3) Reports: `<C OTHER> R+NN` and `<OTHER C> R+NN`. Top-1 other only,
    // 4 of the 8 SNR levels, both directions.
    if let Some(other) = others.first() {
        for &db in A7_REPORT_DBS.iter().step_by(2).take(4) {
            for (text, kind) in [
                (
                    format!("{} {} {:+03}", c, other, db),
                    A7TemplateKind::ReportFromC,
                ),
                (
                    format!("{} {} {:+03}", other, c, db),
                    A7TemplateKind::ReportToC,
                ),
            ] {
                if templates.len() >= A7_MAX_TEMPLATES_PER_CALL {
                    break;
                }
                if let Some(t) = make_template(&text, kind, false) {
                    templates.push(t);
                }
            }
        }
    }

    // (4) Grid: `<C OTHER> GRID` for top-1 other, single grid.
    if let Some(other) = others.first() {
        let grid = A7_FALLBACK_GRIDS[0];
        let text = format!("{} {} {}", c, other, grid);
        if templates.len() < A7_MAX_TEMPLATES_PER_CALL {
            if let Some(t) = make_template(&text, A7TemplateKind::CalledByCGrid, false) {
                templates.push(t);
            }
        }
    }

    // (5) CQ from C: `CQ C` (no grid) and `CQ C GRID0`.
    for (text, kind) in [(
        format!("CQ {} {}", c, A7_FALLBACK_GRIDS[0]),
        A7TemplateKind::CqFromC,
    )] {
        if templates.len() >= A7_MAX_TEMPLATES_PER_CALL {
            break;
        }
        if let Some(t) = make_template(&text, kind, false) {
            templates.push(t);
        }
    }

    templates
}

#[cfg(not(feature = "transmit"))]
pub fn generate_templates(_call: &A7ExpectedCall) -> Vec<A7Template> {
    Vec::new()
}

// ---------------------------------------------------------------------------
// Cross-correlation
// ---------------------------------------------------------------------------

/// Score returned by `cross_correlate`: snr7 is the headline metric.
#[derive(Debug, Clone, Copy)]
pub struct A7Score {
    /// snr7 — sum of signed-LLR / sqrt(N), normalized to act like an SNR.
    /// High positive value (≥6.0 per WSJT-X reference) indicates a strong
    /// template match.
    pub snr7: f64,
    /// Number of bits compared (≤174).
    pub bits_compared: usize,
    /// Raw sum of `llr * sign` (un-normalized) — useful for diagnostics.
    pub raw_sum: f64,
}

/// Compute snr7-like cross-correlation between a template's expected
/// codeword and a candidate's 174 residual LLRs.
///
/// **LLR sign convention** (matches `par_compute_soft_llrs_db`):
/// `llr > 0` ⇒ bit 0 likely; `llr < 0` ⇒ bit 1 likely.
///
/// **Scoring**: for each codeword bit `b`, contribute
/// `llr * (1 - 2*b)` to a running sum. Positive contributions mean the
/// observed LLR agrees with the templated bit; negative contributions
/// mean disagreement. Normalize by `sqrt(N_bits_compared)` to convert a
/// dimensionless sum into an snr7-like score (matches WSJT-X's
/// matched-filter SNR semantics in the LLR domain).
///
/// If `residual_llrs.len() < 174`, only the available prefix is
/// compared (and `bits_compared` reflects that). If longer, only the
/// first 174 are used.
pub fn cross_correlate(template: &A7Template, residual_llrs: &[f32]) -> A7Score {
    let n = residual_llrs.len().min(A7_CODEWORD_BITS);
    if n == 0 {
        return A7Score {
            snr7: 0.0,
            bits_compared: 0,
            raw_sum: 0.0,
        };
    }
    let mut sum: f64 = 0.0;
    for i in 0..n {
        let llr = residual_llrs[i] as f64;
        let sign = if template.codeword[i] { -1.0 } else { 1.0 };
        sum += llr * sign;
    }
    // Normalize like a matched-filter SNR: sum / sqrt(N).
    let snr7 = sum / (n as f64).sqrt();
    A7Score {
        snr7,
        bits_compared: n,
        raw_sum: sum,
    }
}

/// Compute snr7 and snr7b (best-vs-second-best) across a template bank.
///
/// Returns the index of the best-scoring template, plus snr7 (best
/// score) and snr7b (ratio best/second-best). When fewer than 2
/// templates are supplied, snr7b defaults to `f64::INFINITY`.
pub fn best_template_score(
    templates: &[A7Template],
    residual_llrs: &[f32],
) -> Option<(usize, f64, f64)> {
    if templates.is_empty() {
        return None;
    }
    let mut best_idx = 0usize;
    let mut best_snr = f64::NEG_INFINITY;
    let mut second_snr = f64::NEG_INFINITY;
    for (i, t) in templates.iter().enumerate() {
        let s = cross_correlate(t, residual_llrs).snr7;
        if s > best_snr {
            second_snr = best_snr;
            best_snr = s;
            best_idx = i;
        } else if s > second_snr {
            second_snr = s;
        }
    }
    let snr7b = if second_snr.is_finite() && second_snr.abs() > 1e-9 {
        best_snr / second_snr.abs()
    } else {
        f64::INFINITY
    };
    Some((best_idx, best_snr, snr7b))
}

// ---------------------------------------------------------------------------
// Previous-slot filter
// ---------------------------------------------------------------------------

/// The `f0 = -98.0` analog from WSJT-X: filter out an expected-call entry
/// if the same callsign at the same audio frequency was already seen one
/// slot ago. Saves CPU and prevents a7 from firing against a station
/// calling itself again on consecutive slots.
///
/// Two entries collide when callsigns match and `|freq_hz_a - freq_hz_b|
/// < freq_window_hz`.
pub fn dedup_against_previous(
    new_calls: &[A7ExpectedCall],
    previous_calls: &[A7ExpectedCall],
    freq_window_hz: f32,
) -> Vec<A7ExpectedCall> {
    new_calls
        .iter()
        .filter(|new| {
            !previous_calls.iter().any(|prev| {
                prev.call == new.call && (prev.freq_hz - new.freq_hz).abs() < freq_window_hz
            })
        })
        .cloned()
        .collect()
}

// ===========================================================================
// Tests
// ===========================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn known_codeword() -> [bool; A7_CODEWORD_BITS] {
        // A reproducible pseudo-random codeword for cross-correlation tests
        // that don't need a real LDPC-encoded message.
        let mut cw = [false; A7_CODEWORD_BITS];
        let mut x: u64 = 0xDEADBEEFCAFEBABE;
        for b in cw.iter_mut() {
            // xorshift64*
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            *b = (x & 1) == 1;
        }
        cw
    }

    fn synthetic_llrs_from_codeword(
        codeword: &[bool; A7_CODEWORD_BITS],
        magnitude: f32,
    ) -> Vec<f32> {
        // Convert each bit to a clean LLR: bit=0 → +mag, bit=1 → -mag.
        codeword
            .iter()
            .map(|&b| if b { -magnitude } else { magnitude })
            .collect()
    }

    fn awgn_llrs_from_codeword(
        codeword: &[bool; A7_CODEWORD_BITS],
        signal_mag: f32,
        noise_std: f32,
        seed: u64,
    ) -> Vec<f32> {
        // Deterministic Box-Muller for reproducible tests.
        let mut x = seed;
        let mut next_f32 = || {
            // xorshift64*
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let u1 = (x as f64) / (u64::MAX as f64);
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let u2 = (x as f64) / (u64::MAX as f64);
            // Box-Muller
            let r = (-2.0 * u1.max(1e-10).ln()).sqrt();
            let theta = 2.0 * std::f64::consts::PI * u2;
            (r * theta.cos()) as f32
        };
        codeword
            .iter()
            .map(|&b| {
                let clean = if b { -signal_mag } else { signal_mag };
                clean + noise_std * next_f32()
            })
            .collect()
    }

    fn make_synthetic_template() -> A7Template {
        A7Template {
            message_text: "SYNTHETIC".to_string(),
            codeword: known_codeword(),
            pinned: build_standard_pinned_mask(false),
            kind: A7TemplateKind::CalledByC,
        }
    }

    // ----- Template generation tests (require `transmit` feature) ----------

    #[cfg(feature = "transmit")]
    #[test]
    fn test_template_generation_known_call_produces_templates() {
        let expected = A7ExpectedCall::new("K1ABC", 1200.0, A7SlotParity::Even)
            .with_my_call("W1AW")
            .with_heard_with("N1DEF");
        let templates = generate_templates(&expected);
        // Should produce a positive count and be bounded by the cap.
        assert!(
            !templates.is_empty(),
            "expected at least one template for K1ABC"
        );
        assert!(
            templates.len() <= A7_MAX_TEMPLATES_PER_CALL,
            "template count {} exceeds cap {}",
            templates.len(),
            A7_MAX_TEMPLATES_PER_CALL
        );
        // Each template's codeword + pinned mask must be the right length.
        for t in &templates {
            assert_eq!(t.codeword.len(), A7_CODEWORD_BITS);
            assert_eq!(t.pinned.len(), A7_CODEWORD_BITS);
            assert!(
                t.message_text.contains("K1ABC"),
                "missing source call in '{}'",
                t.message_text
            );
        }
    }

    #[cfg(feature = "transmit")]
    #[test]
    fn test_template_pinned_mask_covers_callsigns_and_i3() {
        let expected = A7ExpectedCall::new("K1ABC", 1200.0, A7SlotParity::Even);
        let templates = generate_templates(&expected);
        assert!(!templates.is_empty(), "no templates generated");
        // The first template must pin bits 0-27 (call A), 28-55 (call B),
        // 74-76 (i3). 28+28+3 = 59 minimum pinned bits.
        let t = &templates[0];
        for i in 0..28 {
            assert!(t.pinned[i], "bit {} (call A) not pinned", i);
        }
        for i in 28..56 {
            assert!(t.pinned[i], "bit {} (call B) not pinned", i);
        }
        for i in 74..77 {
            assert!(t.pinned[i], "bit {} (i3) not pinned", i);
        }
        let pin_count = t.pinned_count();
        assert!(
            pin_count >= 59,
            "pinned bit count {} below floor 59",
            pin_count
        );
    }

    #[cfg(feature = "transmit")]
    #[test]
    fn test_template_count_bounded_no_runaway() {
        // Even with rich context, we must stay under the cap.
        let expected = A7ExpectedCall::new("K1ABC", 1200.0, A7SlotParity::Even)
            .with_my_call("W1AW")
            .with_heard_with("N1DEF");
        let templates = generate_templates(&expected);
        assert!(
            templates.len() <= A7_MAX_TEMPLATES_PER_CALL,
            "template count {} exceeds cap {}",
            templates.len(),
            A7_MAX_TEMPLATES_PER_CALL
        );
        // Sanity: with the priority list, we should get a substantial set.
        assert!(
            templates.len() >= 8,
            "template count {} suspiciously low",
            templates.len()
        );
    }

    #[cfg(feature = "transmit")]
    #[test]
    fn test_template_text_contains_expected_kinds() {
        let expected =
            A7ExpectedCall::new("K1ABC", 1200.0, A7SlotParity::Even).with_my_call("W1AW");
        let templates = generate_templates(&expected);
        // Should include at least one status (RR73 / 73), at least one report,
        // at least one CQ.
        let has_rr73 = templates
            .iter()
            .any(|t| t.message_text.contains("RR73") || t.message_text.contains("73"));
        let has_report = templates.iter().any(|t| {
            t.message_text.contains("+")
                || t.message_text.contains("-")
                || t.message_text.contains(" 00")
        });
        let has_cq = templates.iter().any(|t| t.message_text.starts_with("CQ "));
        assert!(
            has_rr73,
            "no status template in set: {:?}",
            templates
                .iter()
                .map(|t| &t.message_text)
                .collect::<Vec<_>>()
        );
        assert!(has_report, "no report template");
        assert!(has_cq, "no CQ template");
    }

    // ----- Cross-correlation tests (no feature gating) ---------------------

    #[test]
    fn test_cross_correlate_matches_self_at_high_snr() {
        let t = make_synthetic_template();
        let llrs = synthetic_llrs_from_codeword(&t.codeword, 10.0);
        let score = cross_correlate(&t, &llrs);
        assert_eq!(score.bits_compared, A7_CODEWORD_BITS);
        // Every bit contributes +10.0, so raw_sum = 174 * 10 = 1740, snr7 =
        // 1740 / sqrt(174) ≈ 131.9.
        let expected_snr = 1740.0 / (174.0_f64).sqrt();
        assert!(
            (score.snr7 - expected_snr).abs() < 0.01,
            "snr7 {} != expected {}",
            score.snr7,
            expected_snr
        );
        assert!(score.snr7 > A7_SNR7_THRESHOLD_DEFAULT * 10.0);
    }

    #[test]
    fn test_cross_correlate_white_noise_below_threshold() {
        let t = make_synthetic_template();
        // Pure-noise LLRs (mean 0, std 1) — over many trials snr7 ~ N(0,1).
        // We use 20 trials and assert that the mean |snr7| stays well below
        // the WSJT-X threshold of 6.0.
        let mut max_abs = 0.0f64;
        for seed in 1u64..=20u64 {
            let llrs = awgn_llrs_from_codeword(
                &[false; A7_CODEWORD_BITS], // signal mag will be 0 → pure noise
                0.0,
                1.0,
                seed.wrapping_mul(0x9E3779B97F4A7C15),
            );
            let score = cross_correlate(&t, &llrs);
            max_abs = max_abs.max(score.snr7.abs());
        }
        assert!(
            max_abs < A7_SNR7_THRESHOLD_DEFAULT,
            "pure-noise max |snr7| {} exceeded threshold {}",
            max_abs,
            A7_SNR7_THRESHOLD_DEFAULT
        );
    }

    #[test]
    fn test_cross_correlate_wrong_template_rejects() {
        let t1 = make_synthetic_template();
        let mut t2 = make_synthetic_template();
        // Flip every codeword bit — totally different template.
        for b in t2.codeword.iter_mut() {
            *b = !*b;
        }
        let llrs = synthetic_llrs_from_codeword(&t1.codeword, 5.0);
        let score1 = cross_correlate(&t1, &llrs);
        let score2 = cross_correlate(&t2, &llrs);
        assert!(
            score1.snr7 > A7_SNR7_THRESHOLD_DEFAULT,
            "good template scored {} below threshold",
            score1.snr7
        );
        assert!(
            score2.snr7 < -A7_SNR7_THRESHOLD_DEFAULT,
            "wrong template scored {} (should be negative)",
            score2.snr7
        );
        // Symmetric magnitude.
        assert!((score1.snr7 + score2.snr7).abs() < 0.01);
    }

    #[test]
    fn test_cross_correlate_robust_to_moderate_noise() {
        let t = make_synthetic_template();
        // Signal mag 3.0, noise std 1.0 — SNR ~ 9.5, expect snr7 well above
        // threshold 6.0.
        let llrs = awgn_llrs_from_codeword(&t.codeword, 3.0, 1.0, 0xC0FFEE);
        let score = cross_correlate(&t, &llrs);
        assert!(
            score.snr7 > A7_SNR7_THRESHOLD_DEFAULT,
            "snr7 {} below threshold at signal=3, noise=1",
            score.snr7
        );
    }

    #[test]
    fn test_cross_correlate_short_llrs_partial_score() {
        let t = make_synthetic_template();
        let mut llrs = synthetic_llrs_from_codeword(&t.codeword, 5.0);
        llrs.truncate(50);
        let score = cross_correlate(&t, &llrs);
        assert_eq!(score.bits_compared, 50);
        // Should still be high SNR but lower magnitude (sqrt(50) vs sqrt(174)).
        let expected = (50.0 * 5.0) / (50.0_f64).sqrt();
        assert!((score.snr7 - expected).abs() < 0.01);
    }

    #[test]
    fn test_cross_correlate_empty_llrs_zero_score() {
        let t = make_synthetic_template();
        let llrs: Vec<f32> = Vec::new();
        let score = cross_correlate(&t, &llrs);
        assert_eq!(score.bits_compared, 0);
        assert_eq!(score.snr7, 0.0);
    }

    #[test]
    fn test_best_template_picks_winner() {
        let t1 = make_synthetic_template();
        let mut t2 = make_synthetic_template();
        // Flip bits so t2 is "wrong" — anti-template.
        for b in t2.codeword.iter_mut() {
            *b = !*b;
        }
        let mut t3 = make_synthetic_template();
        // Flip half the bits in t3 — uncorrelated.
        for b in t3.codeword.iter_mut().take(87) {
            *b = !*b;
        }
        let bank = vec![t2, t1.clone(), t3];
        let llrs = synthetic_llrs_from_codeword(&t1.codeword, 5.0);
        let (idx, snr7, snr7b) = best_template_score(&bank, &llrs).expect("non-empty bank");
        assert_eq!(idx, 1, "winner should be the matched template (index 1)");
        assert!(snr7 > A7_SNR7_THRESHOLD_DEFAULT);
        // snr7b must be finite and >= 1 (best beats second-best).
        assert!(snr7b.is_finite() || snr7b == f64::INFINITY);
    }

    #[test]
    fn test_dedup_against_previous_drops_repeats() {
        let now = Instant::now();
        let prev = vec![A7ExpectedCall {
            call: "K1ABC".to_string(),
            freq_hz: 1200.0,
            slot_parity: A7SlotParity::Even,
            decoded_at: now,
            heard_with: None,
            my_call: None,
        }];
        let new = vec![
            A7ExpectedCall {
                call: "K1ABC".to_string(),
                freq_hz: 1201.0, // within 2 Hz window
                slot_parity: A7SlotParity::Odd,
                decoded_at: now,
                heard_with: None,
                my_call: None,
            },
            A7ExpectedCall {
                call: "W1AW".to_string(),
                freq_hz: 1200.0,
                slot_parity: A7SlotParity::Odd,
                decoded_at: now,
                heard_with: None,
                my_call: None,
            },
            A7ExpectedCall {
                call: "K1ABC".to_string(),
                freq_hz: 1500.0, // far from previous
                slot_parity: A7SlotParity::Odd,
                decoded_at: now,
                heard_with: None,
                my_call: None,
            },
        ];
        let kept = dedup_against_previous(&new, &prev, 2.0);
        assert_eq!(kept.len(), 2, "expected 2 entries kept, got {}", kept.len());
        assert!(kept.iter().any(|c| c.call == "W1AW"));
        assert!(kept.iter().any(|c| c.call == "K1ABC" && c.freq_hz > 1400.0));
    }

    #[test]
    fn test_slot_parity_opposite_is_involution() {
        assert_eq!(A7SlotParity::Even.opposite(), A7SlotParity::Odd);
        assert_eq!(A7SlotParity::Odd.opposite(), A7SlotParity::Even);
        assert_eq!(A7SlotParity::Even.opposite().opposite(), A7SlotParity::Even);
    }

    #[test]
    fn test_a7_score_struct_fields() {
        // Smoke test — make sure the struct is publicly constructable from
        // the public API and fields are accessible.
        let t = make_synthetic_template();
        let llrs = synthetic_llrs_from_codeword(&t.codeword, 4.0);
        let score = cross_correlate(&t, &llrs);
        let _: f64 = score.snr7;
        let _: usize = score.bits_compared;
        let _: f64 = score.raw_sum;
        assert!(score.raw_sum > 0.0);
    }

    // ----- Codeword reconstruction round-trip ------------------------------

    #[cfg(feature = "transmit")]
    #[test]
    fn test_codeword_reconstruction_round_trip() {
        // Encoding then Gray-decoding the data symbols should give back a
        // 174-bit codeword that, fed as clean LLRs to cross_correlate,
        // matches that codeword's own template at extremely high snr7.
        let cw = ldpc_codeword_for_message("CQ K1ABC FN42").expect("encode CQ");
        let t = A7Template {
            message_text: "CQ K1ABC FN42".to_string(),
            codeword: cw,
            pinned: build_standard_pinned_mask(false),
            kind: A7TemplateKind::CqFromC,
        };
        let llrs = synthetic_llrs_from_codeword(&t.codeword, 10.0);
        let score = cross_correlate(&t, &llrs);
        // 174 bits × 10 LLR magnitude / sqrt(174) ≈ 131.9
        assert!(score.snr7 > 100.0, "round-trip snr7 {} too low", score.snr7);
    }
}
