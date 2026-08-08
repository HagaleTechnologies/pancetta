//! Per-bit unsatisfied-check counts — the localized syndrome signal.
//!
//! The decoder already answers two syndrome questions: "are all 83 parity
//! checks satisfied?" ([`crate::decoder`]'s `check_syndrome_fast`, a bool)
//! and "how many are unsatisfied?" (`count_parity_errors`, a scalar total).
//! Neither says *where*. This module scatters each unsatisfied check back
//! onto the codeword bits that participate in it, producing
//! `s = H · hard(LLR)` accumulated per bit.
//!
//! Why this is worth a module: PAN-9's Session-1 separability study measured
//! `parity_errors_final` — that same signal, but as a scalar — as by far the
//! most separable feature it found (d′ **1.65** vs 0.31 for the runner-up),
//! and it is entirely absent from the neural-OSD model's input today. Per-bit
//! counts localize it to the bits OSD actually has to order.
//!
//! Every codeword bit participates in exactly three parity checks (the FT8
//! LDPC(174,91) Mn table, [`crate::ldpc::LDPC_MN`], is `[[u8; 3]; 174]`), so
//! a count is always in `0..=`[`MAX_INCIDENT_CHECKS`].
//!
//! Read-only and additive: nothing here touches BP, the LLR extraction, sync,
//! or any pass structure. It reads the static WSJT-X tables directly rather
//! than a [`crate::ldpc::ParityCheckMatrix`] so it allocates nothing and can
//! be called from the OSD path without a heap hit.

use crate::ldpc::{
    LDPC_CODEWORD_BITS, LDPC_MN, LDPC_NM, LDPC_NUM_CHECKS, LDPC_NUM_ROWS, LDPC_PARITY_BITS,
};

/// Number of parity checks every codeword bit participates in. Fixed by the
/// FT8 LDPC(174,91) code — [`LDPC_MN`] is `[[u8; 3]; 174]`.
pub const MAX_INCIDENT_CHECKS: u8 = 3;

/// Per-bit unsatisfied-check counts for a hard-decided codeword.
///
/// `hard_bits[i]` must be 0 or 1. Returns, for each of the 174 codeword bits,
/// how many of its three incident parity checks are unsatisfied — so `0` means
/// "every check this bit takes part in is happy" and
/// [`MAX_INCIDENT_CHECKS`] means "all three are broken."
///
/// A valid codeword yields all zeros.
pub fn unsatisfied_check_counts(hard_bits: &[u8; LDPC_CODEWORD_BITS]) -> [u8; LDPC_CODEWORD_BITS] {
    // Pass 1: evaluate each check over its incident variables.
    let mut check_unsatisfied = [false; LDPC_NUM_CHECKS];
    for (check_idx, unsatisfied) in check_unsatisfied.iter_mut().enumerate() {
        let num_active = LDPC_NUM_ROWS[check_idx] as usize;
        let mut parity = 0u8;
        // Nm is 1-origin into the variable nodes; 0 is the padding slot.
        for &var_1origin in &LDPC_NM[check_idx][..num_active] {
            if var_1origin > 0 {
                parity ^= hard_bits[var_1origin as usize - 1] & 1;
            }
        }
        *unsatisfied = parity != 0;
    }

    // Pass 2: scatter each unsatisfied check onto its incident bits. Mn is the
    // transpose view — for bit `v`, the (1-origin) checks that reference it.
    let mut counts = [0u8; LDPC_CODEWORD_BITS];
    for (var_idx, count) in counts.iter_mut().enumerate() {
        for &check_1origin in &LDPC_MN[var_idx] {
            if check_1origin > 0 && check_unsatisfied[check_1origin as usize - 1] {
                *count += 1;
            }
        }
    }
    counts
}

/// [`unsatisfied_check_counts`] over LLRs, hard-deciding `llr < 0.0 => 1`.
///
/// The hard-decision convention matches the decoder's existing
/// `check_syndrome_fast` / `count_parity_errors` exactly; the two must agree
/// bit-for-bit or the counts describe a different codeword than the gate that
/// admitted it.
pub fn unsatisfied_check_counts_from_llrs(
    llrs: &[f32; LDPC_CODEWORD_BITS],
) -> [u8; LDPC_CODEWORD_BITS] {
    let mut hard = [0u8; LDPC_CODEWORD_BITS];
    for (bit, &llr) in hard.iter_mut().zip(llrs.iter()) {
        if llr < 0.0 {
            *bit = 1;
        }
    }
    unsatisfied_check_counts(&hard)
}

/// Scale raw counts to `[0.0, 1.0]` for use as a model input row.
///
/// **The divisor is a cross-language contract.** The Python trainer normalizes
/// the syndrome channel the same way; a divisor that disagrees between the two
/// sides is one of the two silent failure modes PAN-9 is built to avoid (it
/// looks identical to "the new objective just doesn't help"). The single
/// source of truth is `training/neural_osd/input_contract.json`.
pub fn normalize_counts(counts: &[u8; LDPC_CODEWORD_BITS]) -> [f32; LDPC_CODEWORD_BITS] {
    let mut out = [0.0f32; LDPC_CODEWORD_BITS];
    for (slot, &c) in out.iter_mut().zip(counts.iter()) {
        *slot = f32::from(c) / f32::from(MAX_INCIDENT_CHECKS);
    }
    out
}

/// Total unsatisfied parity checks, recovered from per-bit counts.
///
/// Each unsatisfied check contributes to exactly `LDPC_NUM_ROWS[check]` bits,
/// so the counts alone do not divide down to a total; this recomputes the
/// checks directly. Present so tests (and the corpus generator) can assert
/// agreement with the decoder's `count_parity_errors` without reaching into
/// the decoder's private methods.
pub fn unsatisfied_check_total(hard_bits: &[u8; LDPC_CODEWORD_BITS]) -> usize {
    let mut total = 0;
    for check_idx in 0..LDPC_NUM_CHECKS {
        let num_active = LDPC_NUM_ROWS[check_idx] as usize;
        let mut parity = 0u8;
        for &var_1origin in &LDPC_NM[check_idx][..num_active] {
            if var_1origin > 0 {
                parity ^= hard_bits[var_1origin as usize - 1] & 1;
            }
        }
        if parity != 0 {
            total += 1;
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ldpc::{LdpcEncoder, LDPC_INFO_BITS};
    use bitvec::prelude::*;

    /// Encode a deterministic 91-bit message into a valid 174-bit codeword.
    fn valid_codeword() -> [u8; LDPC_CODEWORD_BITS] {
        let mut message = bitvec![0; LDPC_INFO_BITS];
        // An arbitrary but non-degenerate pattern — all-zeros is a codeword
        // for any linear code and would pass even a broken implementation.
        for i in 0..LDPC_INFO_BITS {
            message.set(i, (i * 7 + 3) % 5 < 2);
        }
        let encoded = LdpcEncoder::new().encode(&message).expect("encode");
        assert_eq!(encoded.len(), LDPC_CODEWORD_BITS);
        let mut cw = [0u8; LDPC_CODEWORD_BITS];
        for (i, slot) in cw.iter_mut().enumerate() {
            *slot = u8::from(encoded[i]);
        }
        cw
    }

    #[test]
    fn valid_codeword_has_all_zero_counts() {
        let counts = unsatisfied_check_counts(&valid_codeword());
        assert!(
            counts.iter().all(|&c| c == 0),
            "a valid codeword must satisfy every check; got {:?}",
            counts.iter().filter(|&&c| c != 0).count()
        );
        assert_eq!(unsatisfied_check_total(&valid_codeword()), 0);
    }

    #[test]
    fn all_zero_codeword_is_valid() {
        // The zero word is a codeword of any linear code — a sanity floor.
        let counts = unsatisfied_check_counts(&[0u8; LDPC_CODEWORD_BITS]);
        assert!(counts.iter().all(|&c| c == 0));
    }

    #[test]
    fn single_bit_flip_breaks_all_three_incident_checks() {
        for flipped in [0usize, 1, 45, 90, 91, 173] {
            let mut cw = valid_codeword();
            cw[flipped] ^= 1;
            let counts = unsatisfied_check_counts(&cw);

            // The flipped bit sits in exactly 3 checks and it broke all of
            // them, because each was satisfied before the flip.
            assert_eq!(
                counts[flipped], MAX_INCIDENT_CHECKS,
                "bit {flipped}: flipping one bit must break all 3 of its checks"
            );

            // Exactly 3 checks are unsatisfied, no more.
            assert_eq!(unsatisfied_check_total(&cw), 3, "bit {flipped}");

            // Its check-neighbourhood carries nonzero counts, and nothing
            // outside that neighbourhood does.
            let mut neighbourhood = std::collections::HashSet::new();
            for &check_1origin in &LDPC_MN[flipped] {
                if check_1origin > 0 {
                    let check = check_1origin as usize - 1;
                    let active = LDPC_NUM_ROWS[check] as usize;
                    for &v in &LDPC_NM[check][..active] {
                        if v > 0 {
                            neighbourhood.insert(v as usize - 1);
                        }
                    }
                }
            }
            for (bit, &c) in counts.iter().enumerate() {
                if c > 0 {
                    assert!(
                        neighbourhood.contains(&bit),
                        "bit {bit} is outside the flipped bit's check neighbourhood but has count {c}"
                    );
                }
            }
        }
    }

    #[test]
    fn counts_never_exceed_incident_check_count() {
        // Adversarial input: every bit set. Counts are still structurally
        // bounded by the 3 incident checks per bit.
        let counts = unsatisfied_check_counts(&[1u8; LDPC_CODEWORD_BITS]);
        assert!(counts.iter().all(|&c| c <= MAX_INCIDENT_CHECKS));
    }

    #[test]
    fn nonzero_counts_iff_unsatisfied_checks_exist() {
        let valid = valid_codeword();
        assert_eq!(
            unsatisfied_check_counts(&valid).iter().any(|&c| c > 0),
            unsatisfied_check_total(&valid) > 0
        );

        let mut broken = valid;
        broken[17] ^= 1;
        assert_eq!(
            unsatisfied_check_counts(&broken).iter().any(|&c| c > 0),
            unsatisfied_check_total(&broken) > 0
        );
        assert!(unsatisfied_check_counts(&broken).iter().any(|&c| c > 0));
    }

    #[test]
    fn llr_variant_agrees_with_hard_decision_variant() {
        let cw = valid_codeword();
        let mut broken = cw;
        broken[3] ^= 1;
        broken[120] ^= 1;

        // Map bits to LLRs using the decoder's convention: negative == 1.
        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for (i, &b) in broken.iter().enumerate() {
            llrs[i] = if b == 1 { -2.5 } else { 2.5 };
        }
        assert_eq!(
            unsatisfied_check_counts_from_llrs(&llrs),
            unsatisfied_check_counts(&broken)
        );
    }

    #[test]
    fn llr_zero_hard_decides_to_zero() {
        // `llr < 0.0` is strict, so exactly 0.0 decides to bit 0 — same as
        // the decoder's `check_syndrome_fast`/`count_parity_errors`.
        let counts_zero_llrs = unsatisfied_check_counts_from_llrs(&[0.0; LDPC_CODEWORD_BITS]);
        let counts_zero_bits = unsatisfied_check_counts(&[0u8; LDPC_CODEWORD_BITS]);
        assert_eq!(counts_zero_llrs, counts_zero_bits);
    }

    #[test]
    fn normalization_is_in_unit_range_and_hits_both_ends() {
        let mut counts = [0u8; LDPC_CODEWORD_BITS];
        counts[0] = 0;
        counts[1] = 1;
        counts[2] = 2;
        counts[3] = MAX_INCIDENT_CHECKS;
        let norm = normalize_counts(&counts);
        assert!(norm.iter().all(|&v| (0.0..=1.0).contains(&v)));
        assert_eq!(norm[0], 0.0);
        assert_eq!(norm[3], 1.0);
        assert!((norm[1] - 1.0 / 3.0).abs() < 1e-6);
        assert!((norm[2] - 2.0 / 3.0).abs() < 1e-6);
    }

    #[test]
    fn parity_bits_are_covered_too() {
        // The counts array spans the whole codeword, not just the info bits —
        // OSD orders all 174 columns.
        assert_eq!(
            unsatisfied_check_counts(&[0u8; LDPC_CODEWORD_BITS]).len(),
            LDPC_INFO_BITS + LDPC_PARITY_BITS
        );
    }
}
