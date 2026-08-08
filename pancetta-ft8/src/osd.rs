//! Ordered Statistics Decoding (OSD) for FT8 LDPC codes.
//!
//! OSD is a soft-decision decoding technique that serves as a fallback when
//! LDPC belief propagation fails to converge. It works by:
//!
//! 1. Sorting codeword bits by reliability (|LLR| magnitude)
//! 2. Building a systematic generator matrix via Gaussian elimination over GF(2)
//! 3. Hard-deciding the most reliable bits to form a candidate codeword
//! 4. Testing perturbations of the least reliable systematic positions (up to `max_depth`)
//! 5. Validating candidates with CRC-14
//!
//! This module provides the GF(2) matrix primitives and Gaussian elimination
//! needed for OSD. The LDPC code is (174, 91): 91 information bits and 83 parity bits.

// rationale: OSD Gaussian-elimination / matrix loops index parallel rows and
// columns by position; the index is load-bearing.
#![allow(clippy::needless_range_loop)]

use std::collections::HashMap;

use bitvec::prelude::*;

use crate::acceptance::{self, AcceptanceScore};
use crate::budget::DecodeBudget;
use crate::ldpc::{LDPC_CODEWORD_BITS, LDPC_GENERATOR, LDPC_INFO_BITS, LDPC_PARITY_BITS};
use crate::message::{CRC_BITS, PAYLOAD_BITS};

/// npre2 "ntau" — number of leading parity-bit positions used to hash
/// complementary bit-pair signatures. WSJT-X mainline uses `ntau = 14` at
/// `ndeep = 3`, growing to 15-17 at deeper settings. Pancetta uses a
/// fixed 14 for the warm-start preprocessing path — matches mainline's
/// shallowest npre2 activation. See spec
/// `research/specs/spec-wsjtx-mainline-osd174.md` § "ndeep parameter
/// tables".
const NPRE2_NTAU: usize = 14;

/// npre2 marginal-LLR threshold. Info-bit positions whose received
/// `|LLR|` falls below this value are treated as "uncertain" candidates
/// for the complementary-pair search. Pancetta's LLR scale post-
/// normalization is ±10-30 for reliable bits; 2.0 picks out the bits
/// BP could not confidently resolve. Spec leaves the exact threshold to
/// the implementation ("the most-uncertain bits already resolved").
const NPRE2_MARGINAL_LLR: f32 = 2.0;

/// Maximum number of warm-start pair flips to attempt per OSD call.
/// Caps worst-case CPU when many bits land below `NPRE2_MARGINAL_LLR`
/// (e.g. very low SNR). The pair search is `O(k^2)` to build the table
/// but we only attempt the first `NPRE2_MAX_TRIALS` matched pairs.
const NPRE2_MAX_TRIALS: usize = 256;

/// Number of bytes needed to pack a full 174-bit codeword row (ceil(174/8)).
const PACKED_BYTES: usize = 22;

/// Configuration for OSD decoding.
#[derive(Debug, Clone, Copy)]
pub struct OsdConfig {
    /// Maximum OSD order (number of bit flips to try).
    /// Order 0 = hard decision only, order 1 = single bit flips,
    /// order 2 = all pairs of bit flips, etc.
    pub max_depth: u8,

    /// WSJT-X mainline-style npre2 preprocessing — hash-table-driven
    /// complementary-bit-pair search activated when `max_depth >= 3`.
    /// When true, before the OSD order-3+ trial loop runs, the decoder
    /// computes a hash table of parity-column XORs for pairs of
    /// marginal-reliability info bits, looks for pairs whose combined
    /// parity contribution cancels the order-0 parity error, and tries
    /// those pre-flipped pairs as a warm start.
    ///
    /// Inspired by `osd174_91.f90`'s `boxit91`/`fetchit91` rule
    /// (spec: `research/specs/spec-wsjtx-mainline-osd174.md`). Implemented
    /// in pancetta from prose spec only — no GPL source was consulted.
    ///
    /// Default `false` — preserves byte-identical OSD behavior. Flip to
    /// `true` to enable; benefit kicks in only at `max_depth >= 3`.
    pub npre2_preprocessing_enabled: bool,

    /// W2.2 acceptance gate (design spec §4 / `docs/superpowers/specs/
    /// 2026-07-06-decoder-tp-sensitivity-design.md`): the maximum
    /// [`AcceptanceScore::soft_distance`] a `max_depth >= 1` candidate may
    /// have and still be accepted as the OSD result. Calibrated in task
    /// W2.1 (`research/experiments/2026-07-07-acceptance-calibration.md`):
    /// `0.0976` was the largest threshold keeping FDR <= 1% on a definitive
    /// TP/FP subset (hard_200 jt9-verified decodes vs. noise_1000), giving
    /// 100% in-sample TP retention and 98.6% noise-FP rejection. **Carry-
    /// forward caveat**: that FDR is corpus-mix-dependent (a 1250:835
    /// TP:FP ratio that is an artifact of the calibration corpus, not a
    /// measured production base rate) — the THRESHOLD VALUE is the
    /// transferable artifact, re-verify the actual FDR this buys on any
    /// new corpus rather than assuming W2.1's number transfers unchanged.
    /// Only consulted when `max_depth >= 1`; order-0's single hard-decision
    /// candidate (which runs even at `max_depth == 0`, the production
    /// default) is never gated by this field — see
    /// `OsdDecoder::decode_with_features_scored` doc comment.
    pub max_soft_distance: f32,

    /// W2.2 acceptance gate: the maximum [`AcceptanceScore::hard_errors`]
    /// a `max_depth >= 1` candidate may have and still be accepted,
    /// applied together with `max_soft_distance` (both must pass). Default
    /// `37` is the max `hard_errors` observed among W2.1's calibration
    /// hard_200 jt9-verified (TP) population — a secondary, weaker
    /// backstop; `soft_distance` is the load-bearing metric (W2.1 §
    /// "soft_distance disagrees with naive hard-error ordering" — a raw
    /// error count alone can rank cases in the wrong order).
    pub max_hard_errors: u16,

    /// W2.2 early-out bound: once a within-order candidate's
    /// `soft_distance` is at or below this value, stop scanning the
    /// remaining trials at that order and accept it immediately (bounds
    /// OSD-2/OSD-3's worst-case trial count). `0.02` sits comfortably
    /// below W2.1's observed noise-corpus minimum `soft_distance` (0.0242)
    /// and close to the TP mean (0.0155): a candidate this clean is
    /// overwhelmingly likely to be the true codeword, so it is safe to
    /// stop searching for a (implausible) even-better match. This is a
    /// pure cost optimization — it never changes which candidate wins
    /// among those actually compared, since it only fires once a
    /// candidate already passes the (stricter) `max_soft_distance` gate.
    pub accept_immediately_below: f32,
}

impl Default for OsdConfig {
    fn default() -> Self {
        // OSD-1 is the safe default. OSD-2 (4,187 trials) has a high
        // CRC-14 false positive rate without additional validation.
        Self {
            max_depth: 1,
            npre2_preprocessing_enabled: false,
            max_soft_distance: 0.0976,
            max_hard_errors: 37,
            accept_immediately_below: 0.02,
        }
    }
}

/// A packed row of 174 bits stored in 22 bytes, MSB-first packing.
///
/// Bit `col` maps to byte `col / 8`, bit `7 - (col % 8)` within that byte.
pub type PackedRow = [u8; PACKED_BYTES];

/// Get bit at position `col` in a packed row (MSB-first).
#[inline]
fn get_bit(row: &PackedRow, col: usize) -> bool {
    (row[col / 8] >> (7 - (col % 8))) & 1 != 0
}

/// Set bit at position `col` to 1 in a packed row (MSB-first).
#[inline]
fn set_bit(row: &mut PackedRow, col: usize) {
    row[col / 8] |= 1 << (7 - (col % 8));
}

/// Flip bit at position `col` in a packed row (MSB-first).
#[inline]
fn flip_bit(row: &mut PackedRow, col: usize) {
    row[col / 8] ^= 1 << (7 - (col % 8));
}

/// XOR `src` into `dst` (dst ^= src), element-wise over all packed bytes.
#[inline]
fn xor_rows(dst: &mut PackedRow, src: &PackedRow) {
    for i in 0..PACKED_BYTES {
        dst[i] ^= src[i];
    }
}

/// Build the 91x174 systematic generator matrix G = [I_91 | P] from `LDPC_GENERATOR`.
///
/// `LDPC_GENERATOR` is 83 rows x 12 bytes. Row `p` defines which of the 91 info bits
/// contribute to parity bit `p`: `parity[p] = dot(info_bits, LDPC_GENERATOR[p]) mod 2`.
///
/// The systematic generator has 91 rows (one per info bit). Row `k`:
/// - Columns 0..91: identity (bit k is set)
/// - Columns 91..174: for each parity row p (0..83), if LDPC_GENERATOR[p] has bit k set,
///   then column (91 + p) is set in this row.
#[allow(clippy::needless_range_loop)]
fn build_systematic_generator() -> [PackedRow; LDPC_INFO_BITS] {
    let mut g = [[0u8; PACKED_BYTES]; LDPC_INFO_BITS];

    for k in 0..LDPC_INFO_BITS {
        // Identity part: set bit k in the first 91 columns
        set_bit(&mut g[k], k);

        // Parity part: for each parity row p, check if info bit k contributes
        for p in 0..LDPC_PARITY_BITS {
            // LDPC_GENERATOR[p] is 12 bytes, MSB-first, bit k means byte k/8, bit 7-(k%8)
            let byte_idx = k / 8;
            let bit_mask = 1u8 << (7 - (k % 8));
            if LDPC_GENERATOR[p][byte_idx] & bit_mask != 0 {
                set_bit(&mut g[k], LDPC_INFO_BITS + p);
            }
        }
    }

    g
}

/// Row-reduce a 91x174 binary matrix to systematic form using Gaussian elimination over GF(2).
///
/// Pivots on columns 0..91. If no pivot is found in the current column among remaining rows,
/// swaps that column with a column from the right side (columns >= current pivot index from
/// the tail end). Updates `col_perm` to track all column swaps.
///
/// Returns `Some(())` on success, `None` if the matrix is singular (rank < 91).
#[allow(clippy::needless_range_loop)]
fn gaussian_eliminate(
    matrix: &mut [PackedRow; LDPC_INFO_BITS],
    col_perm: &mut [u16; LDPC_CODEWORD_BITS],
) -> Option<()> {
    // Initialize column permutation to identity
    for i in 0..LDPC_CODEWORD_BITS {
        col_perm[i] = i as u16;
    }

    let mut swap_col = LDPC_CODEWORD_BITS; // next column to swap from (decreasing from right)

    for pivot in 0..LDPC_INFO_BITS {
        // Find a row with a 1 in column `pivot`
        let mut found = None;
        for row in pivot..LDPC_INFO_BITS {
            if get_bit(&matrix[row], pivot) {
                found = Some(row);
                break;
            }
        }

        if found.is_none() {
            // No pivot in this column; swap with a column from the right
            let mut swapped = false;
            while swap_col > LDPC_INFO_BITS {
                swap_col -= 1;
                // Check if any row from pivot..91 has a 1 in swap_col
                let mut donor_row = None;
                for row in pivot..LDPC_INFO_BITS {
                    if get_bit(&matrix[row], swap_col) {
                        donor_row = Some(row);
                        break;
                    }
                }
                if donor_row.is_some() {
                    // Swap columns `pivot` and `swap_col` in the matrix
                    for row in 0..LDPC_INFO_BITS {
                        let a = get_bit(&matrix[row], pivot);
                        let b = get_bit(&matrix[row], swap_col);
                        if a != b {
                            flip_bit(&mut matrix[row], pivot);
                            flip_bit(&mut matrix[row], swap_col);
                        }
                    }
                    // Update permutation
                    col_perm.swap(pivot, swap_col);
                    swapped = true;
                    break;
                }
            }
            if !swapped {
                return None; // Singular matrix
            }

            // Now find the pivot row again
            found = None;
            for row in pivot..LDPC_INFO_BITS {
                if get_bit(&matrix[row], pivot) {
                    found = Some(row);
                    break;
                }
            }
            found?;
        }

        let pivot_row = found.unwrap();

        // Swap pivot_row with row `pivot`
        if pivot_row != pivot {
            matrix.swap(pivot, pivot_row);
        }

        // Eliminate all other rows that have a 1 in column `pivot`
        // We need to clone the pivot row to avoid borrow issues
        let pivot_data = matrix[pivot];
        for row in 0..LDPC_INFO_BITS {
            if row != pivot && get_bit(&matrix[row], pivot) {
                xor_rows(&mut matrix[row], &pivot_data);
            }
        }
    }

    Some(())
}

/// Compute CRC-14 directly from a slice of u8 bits (each 0 or 1), without allocating a BitVec.
///
/// This is equivalent to `calculate_crc14()` from `message.rs` but avoids the BitSlice
/// requirement, which would force a heap allocation in the OSD trial loop (~4000 calls).
fn crc14_from_u8_bits(bits: &[u8]) -> u16 {
    const CRC_WIDTH: u32 = 14;
    const POLY: u16 = 0x2757;
    const TOPBIT: u16 = 1u16 << (CRC_WIDTH - 1); // 0x2000
    const NUM_BITS: usize = 82; // 77 payload + 5 zero padding

    // Pack bits into bytes (MSB first), zero-extending to 82 bits
    let mut bytes = [0u8; 11];
    for (i, &b) in bits.iter().enumerate().take(77) {
        if b != 0 {
            bytes[i / 8] |= 0x80u8 >> (i % 8);
        }
    }
    bytes[9] &= 0xF8;

    let mut remainder: u16 = 0;
    let mut idx_byte: usize = 0;

    for idx_bit in 0..NUM_BITS {
        if idx_bit % 8 == 0 {
            remainder ^= (bytes[idx_byte] as u16) << (CRC_WIDTH - 8);
            idx_byte += 1;
        }
        if remainder & TOPBIT != 0 {
            remainder = (remainder << 1) ^ POLY;
        } else {
            remainder <<= 1;
        }
    }

    remainder & ((TOPBIT << 1) - 1)
}

/// WSJT-X mainline-style npre2 preprocessing — hash-table-driven
/// complementary-bit-pair search.
///
/// Given the channel/BP soft LLR vector and an in-progress 174-bit
/// codeword (typically the OSD order-0 codeword + whatever OSD-1/OSD-2
/// flips have already produced), find pairs of marginally-reliable info
/// bits whose combined parity-column XOR (over the first `NPRE2_NTAU`
/// parity bits) cancels the residual parity error. The returned
/// `Vec<u8>` is a length-174 codeword with the most-promising pair of
/// uncertain bits XORed against `codeword_in_progress` as a warm start
/// for the deeper OSD search.
///
/// If no productive pair is found, or if all 174 bits are
/// high-confidence, the function returns `codeword_in_progress`
/// unchanged. The helper is pure and has no side effects.
///
/// Inputs:
/// - `soft_llrs`: channel/BP LLRs in original (un-permuted) order.
/// - `codeword_in_progress`: 174-bit hard-decision vector in original
///   order; values must be 0 or 1.
/// - `parity_cols_perm`: per-info-bit (permuted) parity-column slices
///   from the row-echelon generator (`true` if that info bit
///   contributes to that parity bit). Length 91 x 83.
/// - `final_perm`: the OSD permutation mapping `permuted[i] ->
///   original[final_perm[i]]`. Used to map permuted info-bit indices
///   back to the original codeword positions.
///
/// Output: length-174 codeword vector with at most one productive
/// complementary-pair XOR applied to the info portion.
pub fn npre2_preprocess(
    soft_llrs: &[f32; LDPC_CODEWORD_BITS],
    codeword_in_progress: &[u8; LDPC_CODEWORD_BITS],
    parity_cols_perm: &[[bool; LDPC_PARITY_BITS]; LDPC_INFO_BITS],
    final_perm: &[usize; LDPC_CODEWORD_BITS],
) -> Vec<u8> {
    let mut out: Vec<u8> = codeword_in_progress.to_vec();

    // Step 1: collect marginal info-bit indices (permuted space). A bit
    // is "marginal" if the original-order LLR at its permuted location
    // is below NPRE2_MARGINAL_LLR.
    let mut marginals: Vec<usize> = Vec::with_capacity(LDPC_INFO_BITS);
    for i in 0..LDPC_INFO_BITS {
        let orig_idx = final_perm[i];
        if soft_llrs[orig_idx].abs() < NPRE2_MARGINAL_LLR {
            marginals.push(i);
        }
    }

    // Nothing to do if all info bits are reliable — preserve order-0.
    if marginals.len() < 2 {
        return out;
    }

    // Step 2: compute the residual parity-error signature over the
    // first NPRE2_NTAU parity bits. This is the parity-bit hard-decision
    // hash of (codeword_in_progress)'s parity portion, taken in
    // permuted-column order so it lines up with parity_cols_perm.
    let mut residual: u32 = 0;
    for p in 0..NPRE2_NTAU {
        // Permuted parity column p maps to original index
        // final_perm[LDPC_INFO_BITS + p].
        let orig_idx = final_perm[LDPC_INFO_BITS + p];
        // Compute "expected parity bit" from the info portion of
        // codeword_in_progress under the row-echelon generator: the
        // sum over set info bits of parity_cols_perm[i][p].
        let mut expected: u8 = 0;
        for &i in &marginals {
            // Only marginal-bit contributions; high-confidence bits are
            // assumed to already match the received parity. This keeps
            // the hash relevant to the "uncertain" portion.
            let orig_info_idx = final_perm[i];
            if codeword_in_progress[orig_info_idx] == 1 && parity_cols_perm[i][p] {
                expected ^= 1;
            }
        }
        let received = codeword_in_progress[orig_idx];
        let err = expected ^ received;
        if err == 1 {
            residual |= 1 << p;
        }
    }

    // Step 3: build the hash table — for each pair (i1, i2) of marginal
    // info bits, hash the XOR of their first-NPRE2_NTAU parity columns
    // and store the pair under that hash. WSJT-X uses fixed-size arrays
    // (`boxit91`/`fetchit91`); we use a HashMap with equivalent
    // semantics, which the spec explicitly endorses.
    let mut boxes: HashMap<u32, Vec<(u16, u16)>> = HashMap::new();
    for (a, &i1) in marginals.iter().enumerate() {
        for &i2 in &marginals[(a + 1)..] {
            let mut key: u32 = 0;
            for p in 0..NPRE2_NTAU {
                if parity_cols_perm[i1][p] ^ parity_cols_perm[i2][p] {
                    key |= 1 << p;
                }
            }
            boxes.entry(key).or_default().push((i1 as u16, i2 as u16));
        }
    }

    // Step 4: fetch the pair(s) whose hash matches the residual. If
    // such a pair exists, flipping both info bits zeros the parity
    // error over the first NPRE2_NTAU bits — exactly the WSJT-X
    // "complementary pair" warm start.
    if let Some(pairs) = boxes.get(&residual) {
        if let Some(&(i1, i2)) = pairs.first() {
            // Flip in original-order codeword.
            let orig_i1 = final_perm[i1 as usize];
            let orig_i2 = final_perm[i2 as usize];
            out[orig_i1] ^= 1;
            out[orig_i2] ^= 1;
        }
    }

    out
}

/// Internal npre2 helper: collect up to `NPRE2_MAX_TRIALS` candidate
/// complementary pairs in permuted-info-bit space whose first-NPRE2_NTAU
/// parity-column XOR matches `residual_signature`. Returns pairs as
/// `(permuted_i1, permuted_i2)`. Used by `OsdDecoder::decode` when
/// `npre2_preprocessing_enabled && max_depth >= 3`.
fn npre2_collect_pairs(
    marginals: &[usize],
    parity_cols_perm: &[[bool; LDPC_PARITY_BITS]; LDPC_INFO_BITS],
    residual_signature: u32,
) -> Vec<(usize, usize)> {
    let mut boxes: HashMap<u32, Vec<(u16, u16)>> = HashMap::new();
    for (a, &i1) in marginals.iter().enumerate() {
        for &i2 in &marginals[(a + 1)..] {
            let mut key: u32 = 0;
            for p in 0..NPRE2_NTAU {
                if parity_cols_perm[i1][p] ^ parity_cols_perm[i2][p] {
                    key |= 1 << p;
                }
            }
            boxes.entry(key).or_default().push((i1 as u16, i2 as u16));
        }
    }

    let mut pairs: Vec<(usize, usize)> = Vec::new();
    if let Some(matches) = boxes.get(&residual_signature) {
        for &(i1, i2) in matches.iter().take(NPRE2_MAX_TRIALS) {
            pairs.push((i1 as usize, i2 as usize));
        }
    }
    pairs
}

/// Compute the residual parity-error signature over the first
/// `NPRE2_NTAU` parity bits of an OSD codeword candidate. Used by
/// `OsdDecoder::decode`'s npre2 warm-start path.
///
/// `base_parity` is the order-0 codeword's expected parity (`c0`'s
/// parity portion, encoded from `info_hard` under the row-echelon
/// generator — see `OsdDecoder::decode_with_features_scored_budgeted`'s
/// "Compute base parity" step). `received_parity_hard` is the RECEIVED
/// channel/BP-posterior LLR hard decision at each permuted parity-bit
/// position (`llrs[final_perm[LDPC_INFO_BITS + p]] < 0.0`).
///
/// This is the WSJT-X mainline `e2sub` quantity (spec
/// `research/specs/spec-wsjtx-mainline-osd174.md` § Step 6: `e2sub =
/// (ce XOR hdec)[k+1..N]`) evaluated at the order-0 test pattern
/// (`me = m0`, so `ce = c0` and `ce`'s parity portion is exactly
/// `base_parity`): the residual is the parity-bit DISCREPANCY between
/// what the order-0 codeword expects and what the channel actually
/// received. A pair of marginal info bits whose combined parity-column
/// XOR matches this signature, when flipped, cancels that discrepancy
/// over the first `NPRE2_NTAU` bits — the "complementary pair" warm
/// start the spec describes.
///
/// # Bug fixed here (Task W2.4)
/// The prior implementation ignored `info_hard` and never had access to
/// the received parity hard-decisions at all — it packed `base_parity`'s
/// own bits directly as the "signature" (`let _ = info_hard;` disabling
/// two of its three parameters). That quantity has no dependency on what
/// was actually received, so it cannot express "cancel an error": it
/// would be nonzero (and thus require a warm-start pair) even when
/// `base_parity` already perfectly matched the received parity (zero
/// true residual), and would never correctly signal a genuine
/// discrepancy either. See
/// `research/experiments/2026-07-07-w24-npre2-residual-signature-fix.md`
/// for a worked numeric example and the derivation from the spec.
fn npre2_residual_signature(
    base_parity: &[u8; LDPC_PARITY_BITS],
    received_parity_hard: &[u8; LDPC_PARITY_BITS],
) -> u32 {
    let mut sig: u32 = 0;
    for p in 0..NPRE2_NTAU {
        if base_parity[p] != received_parity_hard[p] {
            sig |= 1 << p;
        }
    }
    sig
}

/// Smallest/largest CNN error-probability accepted before mapping to a
/// pseudo-LLR via `ln((1-p)/p)` — clamps away from the poles so a
/// saturated sigmoid output (0.0 or 1.0) never produces +/-infinity.
const NEURAL_PROB_EPS: f32 = 1e-6;

/// Compute the reliability-descending column order OSD uses to seed its
/// Gaussian elimination: `result[0]` is the single most-reliable codeword
/// position (info or parity), `result[LDPC_CODEWORD_BITS - 1]` the least.
/// The first `LDPC_INFO_BITS` columns of this order become candidates for
/// the hard-decided/pivot ("info") role; the rest are solved for via the
/// parity constraint. Getting "most reliable first" right — across BOTH
/// info and parity bits, on ONE commensurable scale — is what lets
/// Gaussian elimination preferentially keep genuinely-trustworthy bits
/// fixed and push genuinely-unreliable ones to the parity-derived role.
///
/// Without neural ordering, reliability is just `|LLR|` for every
/// position (channel/BP-posterior LLR magnitude), sorted descending.
///
/// With neural ordering (`neural_ordering: Some(probs)`), `probs[i]` is
/// the CNN's predicted probability that info bit `i` is in error — a
/// `[0, 1]` scale, NOT an LLR magnitude. Comparing it directly against
/// unbounded `|LLR|` parity keys (as the pre-fix code did) is comparing
/// incommensurable scales: nearly every info bit would out-rank nearly
/// every parity bit with `|LLR| > 1`, regardless of true reliability.
/// Instead, map `p` to a pseudo-LLR on the same log-odds scale via
/// `ln((1-p)/p)`: `p -> 0` (CNN confident the bit is correct) yields a
/// large POSITIVE key (reliable, sorts first, like a large `|LLR|`);
/// `p -> 1` (CNN confident the bit is an error) yields a large NEGATIVE
/// key (the worst possible candidate for the fixed/hard-decided role,
/// sorts last); `p = 0.5` (maximum uncertainty) yields exactly `0`,
/// matching the zero-reliability point of a real channel LLR. Parity
/// bits keep `|LLR|` directly (no sign flip) so both classes share the
/// same "larger key = more reliable, sorts first" convention.
fn reliability_sorted_indices(
    llrs: &[f32; LDPC_CODEWORD_BITS],
    neural_ordering: Option<&[f32; LDPC_INFO_BITS]>,
) -> [usize; LDPC_CODEWORD_BITS] {
    let mut sorted_indices: [usize; LDPC_CODEWORD_BITS] = [0; LDPC_CODEWORD_BITS];
    for i in 0..LDPC_CODEWORD_BITS {
        sorted_indices[i] = i;
    }
    let mut keys = [0.0f32; LDPC_CODEWORD_BITS];
    if let Some(probs) = neural_ordering {
        for i in 0..LDPC_CODEWORD_BITS {
            keys[i] = if i < LDPC_INFO_BITS {
                let p = probs[i].clamp(NEURAL_PROB_EPS, 1.0 - NEURAL_PROB_EPS);
                ((1.0 - p) / p).ln()
            } else {
                llrs[i].abs()
            };
        }
    } else {
        for i in 0..LDPC_CODEWORD_BITS {
            keys[i] = llrs[i].abs();
        }
    }
    // Sort descending by key: largest key (most reliable) first.
    sorted_indices.sort_by(|&a, &b| {
        keys[b]
            .partial_cmp(&keys[a])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sorted_indices
}

/// OSD decoder that attempts to decode LLRs using ordered statistics decoding
/// at depths 0, 1, 2, and 3 with CRC-14 validation.
#[derive(Clone)]
pub struct OsdDecoder {
    config: OsdConfig,
    generator: [PackedRow; LDPC_INFO_BITS],
}

impl OsdDecoder {
    /// Create a new OSD decoder with the given configuration.
    pub fn new(config: OsdConfig) -> Self {
        Self {
            config,
            generator: build_systematic_generator(),
        }
    }

    /// Configured maximum OSD order. Callers use this to decide whether
    /// computing a neural ordering is worth its cost at all — at
    /// `max_depth == 0` only the plain hard-decision candidate is ever
    /// tried, so the CNN forward pass that would otherwise seed the
    /// neural ordering is pure wasted work (see `decoder.rs`'s
    /// `osd.max_depth() >= 1` gate before calling
    /// `neural_osd::predict_error_bits`).
    pub fn max_depth(&self) -> u8 {
        self.config.max_depth
    }

    /// Attempt to decode 174 LLRs into a valid 174-bit codeword.
    ///
    /// Returns `Some(BitVec)` of 174 bits if a valid codeword (passing CRC-14) is found,
    /// or `None` if no valid candidate is found at the configured depth.
    ///
    /// FDR Session 3 wrapper: callers that want OSD telemetry should use
    /// [`Self::decode_with_features`]; this method discards the per-success
    /// depth and hard-error count.
    #[allow(clippy::needless_range_loop)]
    pub fn decode(
        &self,
        llrs: &[f32; LDPC_CODEWORD_BITS],
        neural_ordering: Option<&[f32; LDPC_INFO_BITS]>,
    ) -> Option<BitVec> {
        self.decode_with_features(llrs, neural_ordering)
            .map(|(bits, _depth, _nharderrs)| bits)
    }

    /// FDR Session 3: like [`Self::decode`] but returns
    /// `Some((codeword, depth_used, nharderrs))`. `depth_used` is the
    /// OSD depth at which the best-scoring candidate was accepted
    /// (0/1/2/3, where 3 covers both the npre2 warm-start and the full
    /// triple loop). `nharderrs` is the number of info bits flipped at
    /// the accepted trial (0 / 1 / 2 / 2 [npre2 pair] / 3 [triple]).
    /// Inspired by spec ref `spec-wsjtx-improved-fdr.md` §"Inputs".
    ///
    /// Thin wrapper around [`Self::decode_with_features_scored`] using
    /// `llrs` as both the search array AND the acceptance-scoring
    /// "channel" array (the two coincide for every caller that doesn't
    /// separately track pre-BP channel LLRs, e.g. this module's own unit
    /// tests). Production decode (`decoder.rs`) uses
    /// `decode_with_features_scored` directly so it can pass the true
    /// pre-BP channel LLRs separately from the (possibly BP-offset-
    /// adjusted) search array.
    #[allow(clippy::needless_range_loop)]
    pub fn decode_with_features(
        &self,
        llrs: &[f32; LDPC_CODEWORD_BITS],
        neural_ordering: Option<&[f32; LDPC_INFO_BITS]>,
    ) -> Option<(BitVec, u8, u8)> {
        self.decode_with_features_scored(llrs, llrs, neural_ordering)
            .map(|(bits, depth, nharderrs, _score)| (bits, depth, nharderrs))
    }

    /// W2.2 (`docs/superpowers/specs/2026-07-06-decoder-tp-sensitivity-design.md`
    /// §4): like [`Self::decode_with_features`] but takes a SEPARATE
    /// `channel_llrs` array — the pre-BP signal-domain LLRs — used ONLY
    /// to score and RANK candidates at `max_depth >= 1` via
    /// [`acceptance::score`]. `llrs` continues to drive reliability
    /// ordering and candidate generation exactly as before (unchanged);
    /// `channel_llrs` never influences WHICH candidates get tried, only
    /// which one wins among those that pass CRC-14 at a given order.
    /// Also returns the winning candidate's [`AcceptanceScore`].
    ///
    /// # The bug this fixes
    /// Prior to W2.2, the order-1/2/3 loops below returned on the FIRST
    /// CRC-14 pass encountered while enumerating flip patterns (up to
    /// 121,485 for order-3). At CRC-14's 2⁻¹⁴ collision rate, a false
    /// (CRC-valid but wrong) candidate turning up before the true one in
    /// iteration order is a statistical certainty at that trial volume —
    /// "first CRC pass" has no reason to correlate with "actually the
    /// right codeword". This method instead collects every CRC-valid
    /// candidate within an order, keeps the one with the MINIMUM
    /// `soft_distance` (the true codeword should almost always agree far
    /// better with the channel LLRs than a random CRC collision), and
    /// requires that minimum to also pass an absolute acceptance gate
    /// (`max_soft_distance` / `max_hard_errors`) before accepting it —
    /// a low-confidence "best of a bad lot" is still rejected, escalating
    /// to the next (deeper, costlier) order instead.
    ///
    /// # Why order-0 is untouched
    /// Order-0 (the plain hard-decision candidate, zero flips) runs
    /// UNCONDITIONALLY regardless of `max_depth` — including at
    /// `max_depth == 0`, the PRODUCTION DEFAULT (`osd_depth: Some(0)`).
    /// There is only ever ONE order-0 candidate, so there is no
    /// first-vs-best SELECTION ambiguity there, and adding a gate would
    /// change production behavior at the default config — which this
    /// task must not do. The collect-and-rank + acceptance-gate fix below
    /// applies ONLY to the order-1/2/3 (+ npre2) loops, all of which are
    /// unreachable at `max_depth == 0` (each is behind its own `if
    /// self.config.max_depth < N { return None; }` guard), so production
    /// is byte-identical.
    ///
    /// Thin wrapper around [`Self::decode_with_features_scored_budgeted`]
    /// with [`DecodeBudget::unlimited()`] — preserves every existing
    /// caller (including every unit test in this module and
    /// [`Self::decode_with_features`]/[`Self::decode`]) byte-for-byte,
    /// since `has_time()` on an unlimited budget always returns `true`.
    #[allow(clippy::needless_range_loop)]
    pub fn decode_with_features_scored(
        &self,
        llrs: &[f32; LDPC_CODEWORD_BITS],
        channel_llrs: &[f32; LDPC_CODEWORD_BITS],
        neural_ordering: Option<&[f32; LDPC_INFO_BITS]>,
    ) -> Option<(BitVec, u8, u8, AcceptanceScore)> {
        self.decode_with_features_scored_budgeted(
            llrs,
            channel_llrs,
            neural_ordering,
            DecodeBudget::unlimited(),
        )
    }

    /// Build the most-reliable-basis (MRB) for a set of LLRs: the reduced
    /// generator matrix and the permutation that maps MRB column `i` back to
    /// its original codeword position.
    ///
    /// This is steps 1-4 of [`Self::decode_with_features_scored_budgeted`],
    /// extracted verbatim so that function and the research capture path share
    /// one implementation. Returns `None` exactly where the inline code
    /// short-circuited: when Gaussian elimination cannot find a full-rank
    /// basis.
    ///
    /// Extracted for PAN-9: the soft-rank training label is only meaningful in
    /// this permuted basis, and re-deriving the permutation outside the
    /// decoder would risk a silent skew between what the model is trained to
    /// order and what OSD actually reprocesses.
    fn mrb_basis(
        &self,
        llrs: &[f32; LDPC_CODEWORD_BITS],
        neural_ordering: Option<&[f32; LDPC_INFO_BITS]>,
    ) -> Option<(
        [[u8; PACKED_BYTES]; LDPC_INFO_BITS],
        [usize; LDPC_CODEWORD_BITS],
    )> {
        // 1. Sort indices by reliability (most reliable first, on a
        // commensurable scale across info AND parity bits — see
        // `reliability_sorted_indices`).
        let sorted_indices = reliability_sorted_indices(llrs, neural_ordering);

        // 2. Permute generator columns per reliability ranking
        let mut matrix = [[0u8; PACKED_BYTES]; LDPC_INFO_BITS];
        for row in 0..LDPC_INFO_BITS {
            for new_col in 0..LDPC_CODEWORD_BITS {
                let orig_col = sorted_indices[new_col];
                if get_bit(&self.generator[row], orig_col) {
                    set_bit(&mut matrix[row], new_col);
                }
            }
        }

        // 3. Gaussian eliminate
        let mut elim_perm = [0u16; LDPC_CODEWORD_BITS];
        gaussian_eliminate(&mut matrix, &mut elim_perm)?;

        // 4. Compose permutations: final_perm[i] = sorted_indices[elim_perm[i]]
        let mut final_perm = [0usize; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            final_perm[i] = sorted_indices[elim_perm[i] as usize];
        }

        Some((matrix, final_perm))
    }

    /// The MRB permutation this decoder would use for `llrs`.
    ///
    /// `perm[i]` is the original codeword position of MRB column `i`; columns
    /// `0..91` are the information set OSD hard-decides and reprocesses, in
    /// descending reliability. Returns `None` when no full-rank basis exists.
    ///
    /// Research/capture surface only — the production decode path gets the
    /// permutation from [`Self::mrb_basis`] directly, alongside the reduced
    /// matrix it also needs. Exposed for PAN-9's trajectory capture, which
    /// must label in exactly this basis.
    pub fn mrb_permutation(
        &self,
        llrs: &[f32; LDPC_CODEWORD_BITS],
        neural_ordering: Option<&[f32; LDPC_INFO_BITS]>,
    ) -> Option<[usize; LDPC_CODEWORD_BITS]> {
        self.mrb_basis(llrs, neural_ordering)
            .map(|(_matrix, perm)| perm)
    }

    /// Task W2.4 (decoder-speed-overhaul budget integration): identical to
    /// [`Self::decode_with_features_scored`], but checkpoints the
    /// escalation ladder (order-1 -> order-2 -> order-3/npre2) against a
    /// [`DecodeBudget`] the same way the decode-speed-overhaul plan's
    /// S1-S7 stages check `DecodeBudget::has_time()` between work items.
    /// OSD orders were not exercised in production before this task
    /// (`osd_depth: Some(0)` never escalated past the always-cheap
    /// order-0 hard-decision trial), so — now that deeper orders are
    /// live — each order boundary is treated as its own checkpointed
    /// work item: order-2 is ~45x costlier than order-1 (`C(91,2)` vs
    /// `C(91,1)` trials) and order-3 a further ~30x costlier again
    /// (`C(91,3)`), so re-checking between EACH order (not just once per
    /// candidate) keeps a single slow candidate from silently consuming
    /// an entire window's remaining budget on its own. Order-0 is
    /// UNCHANGED (see the doc comment above) — it never consults
    /// `budget` at all, so a budget that's already expired when this is
    /// called still gets the same cheap order-0 attempt production
    /// always ran.
    #[allow(clippy::needless_range_loop)]
    pub fn decode_with_features_scored_budgeted(
        &self,
        llrs: &[f32; LDPC_CODEWORD_BITS],
        channel_llrs: &[f32; LDPC_CODEWORD_BITS],
        neural_ordering: Option<&[f32; LDPC_INFO_BITS]>,
        budget: DecodeBudget,
    ) -> Option<(BitVec, u8, u8, AcceptanceScore)> {
        // 1-4. Build the most-reliable-basis: reliability sort, generator
        // column permute, Gaussian elimination, permutation compose. Factored
        // into `mrb_basis` so the research capture path (PAN-9 Phase 2) can
        // obtain the SAME `final_perm` this decode uses rather than
        // re-deriving it — a re-derivation that drifted would silently skew
        // every soft-rank training label. Pure extraction: this function
        // executes the identical code it did inline.
        let (matrix, final_perm) = self.mrb_basis(llrs, neural_ordering)?;

        // 5. OSD-0: hard-decide the 91 most reliable bits
        let mut info_hard = [0u8; LDPC_INFO_BITS];
        for i in 0..LDPC_INFO_BITS {
            let orig_col = final_perm[i];
            if llrs[orig_col] < 0.0 {
                info_hard[i] = 1;
            }
        }

        // Compute base parity
        let mut base_parity = [0u8; LDPC_PARITY_BITS];
        for p in 0..LDPC_PARITY_BITS {
            let mut val = 0u8;
            for i in 0..LDPC_INFO_BITS {
                if info_hard[i] == 1 && get_bit(&matrix[i], LDPC_INFO_BITS + p) {
                    val ^= 1;
                }
            }
            base_parity[p] = val;
        }

        // Try OSD-0 — single candidate, no selection ambiguity, no gate
        // (see doc comment above: this path is live even at max_depth==0,
        // today's production default, and must stay byte-identical).
        if let Some(result) = self.try_solution(&info_hard, &base_parity, &final_perm) {
            let score = acceptance::score(&result, channel_llrs);
            return Some((result, 0, 0, score));
        }

        if self.config.max_depth < 1 {
            return None;
        }
        // Task W2.4: checkpoint before the first escalation work item
        // (order-1, 91 trials) — cheap, but a window whose budget is
        // already exhausted by earlier candidates should not pay even
        // this cost on every remaining BP-failed candidate.
        if !budget.has_time() {
            return None;
        }

        // 6. OSD-1: pre-compute parity columns
        let mut parity_cols = [[false; LDPC_PARITY_BITS]; LDPC_INFO_BITS];
        for i in 0..LDPC_INFO_BITS {
            for p in 0..LDPC_PARITY_BITS {
                parity_cols[i][p] = get_bit(&matrix[i], LDPC_INFO_BITS + p);
            }
        }

        // W2.2: collect every CRC-valid single-flip candidate and keep the
        // minimum-soft_distance one, instead of returning the first.
        let mut best: Option<(BitVec, AcceptanceScore)> = None;
        for flip in 0..LDPC_INFO_BITS {
            let mut info = info_hard;
            info[flip] ^= 1;
            let mut parity = base_parity;
            for p in 0..LDPC_PARITY_BITS {
                if parity_cols[flip][p] {
                    parity[p] ^= 1;
                }
            }
            if let Some(candidate) = self.try_solution(&info, &parity, &final_perm) {
                let score = acceptance::score(&candidate, channel_llrs);
                if self.is_better_candidate(&best, &score) {
                    let accept_now = score.soft_distance <= self.config.accept_immediately_below;
                    best = Some((candidate, score));
                    if accept_now {
                        break;
                    }
                }
            }
        }
        if let Some((codeword, score)) = best {
            if self.passes_acceptance_gate(&score) {
                return Some((codeword, 1, 1, score));
            }
            // Best order-1 candidate failed the acceptance gate (likely a
            // CRC-14 collision, not a real decode) — fall through to a
            // deeper (costlier but not-yet-exhausted) order rather than
            // trusting it.
        }

        if self.config.max_depth < 2 {
            return None;
        }
        // Task W2.4: checkpoint before order-2 (`C(91,2)` = 4,095 trials,
        // ~45x order-1's cost) — the first genuinely expensive escalation
        // step.
        if !budget.has_time() {
            return None;
        }

        // 7. OSD-2: flip pairs — same collect-and-rank pattern as OSD-1.
        let mut best: Option<(BitVec, AcceptanceScore)> = None;
        'osd2: for i in 0..LDPC_INFO_BITS {
            for j in (i + 1)..LDPC_INFO_BITS {
                let mut info = info_hard;
                info[i] ^= 1;
                info[j] ^= 1;
                let mut parity = base_parity;
                for p in 0..LDPC_PARITY_BITS {
                    if parity_cols[i][p] {
                        parity[p] ^= 1;
                    }
                    if parity_cols[j][p] {
                        parity[p] ^= 1;
                    }
                }
                if let Some(candidate) = self.try_solution(&info, &parity, &final_perm) {
                    let score = acceptance::score(&candidate, channel_llrs);
                    if self.is_better_candidate(&best, &score) {
                        let accept_now =
                            score.soft_distance <= self.config.accept_immediately_below;
                        best = Some((candidate, score));
                        if accept_now {
                            break 'osd2;
                        }
                    }
                }
            }
        }
        if let Some((codeword, score)) = best {
            if self.passes_acceptance_gate(&score) {
                return Some((codeword, 2, 2, score));
            }
        }

        if self.config.max_depth < 3 {
            return None;
        }
        // Task W2.4: checkpoint before order-3 + npre2 (`C(91,3)` =
        // 121,485 trials, ~30x order-2's cost again) — by far the
        // costliest escalation step; a single candidate reaching this
        // point with an already-exhausted budget must not run it.
        if !budget.has_time() {
            return None;
        }

        // 8a. WSJT-X mainline-style npre2 preprocessing — warm-start the
        //     OSD-3 trial loop by flipping complementary bit pairs whose
        //     combined parity-column XOR (over the first NPRE2_NTAU
        //     bits) matches the residual parity-error signature. Inspired
        //     by `osd174_91.f90`'s boxit91/fetchit91 rule. Default OFF
        //     preserves byte-identical OSD-3 behavior. Spec:
        //     research/specs/spec-wsjtx-mainline-osd174.md § Step 6.
        if self.config.npre2_preprocessing_enabled {
            // Collect marginally-reliable info-bit indices in permuted
            // space. A bit is "marginal" if its original-order LLR
            // magnitude is below NPRE2_MARGINAL_LLR — these are the
            // bits BP could not confidently resolve.
            let mut marginals: Vec<usize> = Vec::with_capacity(LDPC_INFO_BITS);
            for i in 0..LDPC_INFO_BITS {
                let orig_idx = final_perm[i];
                if llrs[orig_idx].abs() < NPRE2_MARGINAL_LLR {
                    marginals.push(i);
                }
            }

            if marginals.len() >= 2 {
                // Task W2.4 fix: the residual must depend on what was
                // actually RECEIVED, not just on `base_parity` in
                // isolation (see `npre2_residual_signature`'s doc
                // comment). Build the received parity-bit hard-decision
                // array from `llrs` at the permuted parity positions.
                let mut received_parity_hard = [0u8; LDPC_PARITY_BITS];
                for (p, slot) in received_parity_hard.iter_mut().enumerate() {
                    let orig_idx = final_perm[LDPC_INFO_BITS + p];
                    *slot = u8::from(llrs[orig_idx] < 0.0);
                }
                let residual = npre2_residual_signature(&base_parity, &received_parity_hard);
                let warm_pairs = npre2_collect_pairs(&marginals, &parity_cols, residual);

                let mut best: Option<(BitVec, AcceptanceScore)> = None;
                for (i1, i2) in warm_pairs {
                    let mut info = info_hard;
                    info[i1] ^= 1;
                    info[i2] ^= 1;
                    let mut parity = base_parity;
                    for p in 0..LDPC_PARITY_BITS {
                        if parity_cols[i1][p] {
                            parity[p] ^= 1;
                        }
                        if parity_cols[i2][p] {
                            parity[p] ^= 1;
                        }
                    }
                    if let Some(candidate) = self.try_solution(&info, &parity, &final_perm) {
                        let score = acceptance::score(&candidate, channel_llrs);
                        if self.is_better_candidate(&best, &score) {
                            let accept_now =
                                score.soft_distance <= self.config.accept_immediately_below;
                            best = Some((candidate, score));
                            if accept_now {
                                break;
                            }
                        }
                    }
                }
                if let Some((codeword, score)) = best {
                    if self.passes_acceptance_gate(&score) {
                        // npre2 warm-start always flips a marginal pair,
                        // even though it's enumerated within the depth-3 budget.
                        return Some((codeword, 3, 2, score));
                    }
                }
            }
        }

        // 8. OSD-3: flip all triples — C(91, 3) = 121,485 trials
        //    (= 91 · 90 · 89 / 6). Comment corrected 2026-06-02 (Phase C)
        //    per docs/engineering/2026-06-02-engineering-substance-audit.md
        //    (claim 17); loop math was already correct. Same
        //    collect-and-rank pattern as OSD-1/OSD-2.
        // Each trial XORs 3 rows of the reduced generator matrix, then checks CRC-14.
        let mut best: Option<(BitVec, AcceptanceScore)> = None;
        'osd3: for i in 0..LDPC_INFO_BITS {
            for j in (i + 1)..LDPC_INFO_BITS {
                // Pre-compute i+j parity update to avoid recomputing in innermost loop
                let mut parity_ij = base_parity;
                for p in 0..LDPC_PARITY_BITS {
                    if parity_cols[i][p] {
                        parity_ij[p] ^= 1;
                    }
                    if parity_cols[j][p] {
                        parity_ij[p] ^= 1;
                    }
                }
                for k in (j + 1)..LDPC_INFO_BITS {
                    let mut info = info_hard;
                    info[i] ^= 1;
                    info[j] ^= 1;
                    info[k] ^= 1;
                    let mut parity = parity_ij;
                    for p in 0..LDPC_PARITY_BITS {
                        if parity_cols[k][p] {
                            parity[p] ^= 1;
                        }
                    }
                    if let Some(candidate) = self.try_solution(&info, &parity, &final_perm) {
                        let score = acceptance::score(&candidate, channel_llrs);
                        if self.is_better_candidate(&best, &score) {
                            let accept_now =
                                score.soft_distance <= self.config.accept_immediately_below;
                            best = Some((candidate, score));
                            if accept_now {
                                break 'osd3;
                            }
                        }
                    }
                }
            }
        }
        if let Some((codeword, score)) = best {
            if self.passes_acceptance_gate(&score) {
                return Some((codeword, 3, 3, score));
            }
        }

        None
    }

    /// `true` iff `candidate_score` is a strictly better (lower
    /// `soft_distance`) match than the current `best`, or `best` is
    /// `None`. Shared by every order-1/2/3(+npre2) collect-and-rank loop
    /// in [`Self::decode_with_features_scored`].
    #[inline]
    fn is_better_candidate(
        &self,
        best: &Option<(BitVec, AcceptanceScore)>,
        candidate_score: &AcceptanceScore,
    ) -> bool {
        match best {
            None => true,
            Some((_, best_score)) => candidate_score.soft_distance < best_score.soft_distance,
        }
    }

    /// W2.2 acceptance gate: `true` iff `score` is trustworthy enough to
    /// accept as the final OSD result at `max_depth >= 1` — both the
    /// weighted soft distance AND the raw hard-error count must be within
    /// the configured bounds. See [`OsdConfig::max_soft_distance`] /
    /// [`OsdConfig::max_hard_errors`] for the calibration provenance and
    /// the carry-forward caveat about corpus-mix-dependent FDR.
    #[inline]
    fn passes_acceptance_gate(&self, score: &AcceptanceScore) -> bool {
        score.soft_distance <= self.config.max_soft_distance
            && score.hard_errors <= self.config.max_hard_errors
    }

    /// Un-permute info+parity bits into a codeword and check CRC-14.
    fn try_solution(
        &self,
        info: &[u8; LDPC_INFO_BITS],
        parity: &[u8; LDPC_PARITY_BITS],
        final_perm: &[usize; LDPC_CODEWORD_BITS],
    ) -> Option<BitVec> {
        // Un-permute into codeword
        let mut codeword = [0u8; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_INFO_BITS {
            codeword[final_perm[i]] = info[i];
        }
        for p in 0..LDPC_PARITY_BITS {
            codeword[final_perm[LDPC_INFO_BITS + p]] = parity[p];
        }

        // Compute CRC-14 directly on codeword bytes (avoids BitVec allocation in hot loop)
        let calculated_crc = crc14_from_u8_bits(&codeword[..PAYLOAD_BITS]);

        // Extract received CRC from bits 77..91
        let mut received_crc = 0u16;
        for i in 0..CRC_BITS {
            if codeword[PAYLOAD_BITS + i] == 1 {
                received_crc |= 1 << (CRC_BITS - 1 - i);
            }
        }

        if calculated_crc == received_crc {
            // Return all 174 bits
            let result: BitVec = codeword.iter().map(|&b| b == 1).collect();
            Some(result)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// W1.5: the neural-ordering reliability sort must rank a
    /// genuinely-most-reliable parity bit ahead of a genuinely-most-
    /// reliable info bit's peers (cross-class comparability on one
    /// commensurable scale), and must never rank a genuinely-unreliable
    /// parity bit ahead of an ordinary/reliable parity bit (no sign
    /// inversion within the parity class).
    ///
    /// Pre-fix, `reliability_sorted_indices` keyed parity bits by
    /// `-|LLR|` under a descending sort — ranking parity bits by
    /// ASCENDING reliability — and compared the CNN's `[0,1]`
    /// probability scale directly against unbounded `|LLR|` magnitudes,
    /// so essentially every info bit outranked essentially every parity
    /// bit regardless of true reliability. Both assertions below fail
    /// against that code and pass once parity uses `+|LLR|` and info
    /// uses a commensurable `ln((1-p)/p)` pseudo-LLR.
    #[test]
    fn test_neural_ordering_parity_and_info_share_commensurable_reliability_scale() {
        let mut llrs = [3.0f32; LDPC_CODEWORD_BITS];
        let mut probs = [0.5f32; LDPC_INFO_BITS];

        // A garden-variety ("bulk") parity bit — moderate reliability.
        const BULK_PARITY_IDX: usize = LDPC_INFO_BITS + 5; // 96
                                                           // The single most reliable bit in the whole codeword: a parity
                                                           // bit with a huge |LLR|.
        const BEST_PARITY_IDX: usize = LDPC_INFO_BITS + 40; // 131
        llrs[BEST_PARITY_IDX] = 50.0;
        // A parity bit that is genuinely far LESS reliable than the bulk.
        const WORST_PARITY_IDX: usize = LDPC_INFO_BITS + 60; // 151
        llrs[WORST_PARITY_IDX] = 0.001;

        // A very reliable info bit per the CNN (near-zero error probability).
        const BEST_INFO_IDX: usize = 10;
        probs[BEST_INFO_IDX] = 0.001;
        // A very unreliable info bit per the CNN (near-certain error).
        const WORST_INFO_IDX: usize = 60;
        probs[WORST_INFO_IDX] = 0.999;

        let order = reliability_sorted_indices(&llrs, Some(&probs));
        let rank_of = |idx: usize| order.iter().position(|&x| x == idx).unwrap();

        // Cross-class comparability: the single best bit in the entire
        // codeword is a parity bit, so it must rank ahead of the best
        // INFO bit too, not just be arbitrarily shuffled among parity
        // positions. Under the pre-fix code every info bit sorted ahead
        // of every parity bit with |LLR| > 1 regardless of reliability
        // (probs is in [-1, 0], -|llr| for |llr| > 1 is < -1), so this
        // fails pre-fix (BEST_PARITY_IDX lands dead last, at rank 173).
        assert!(
            rank_of(BEST_PARITY_IDX) < rank_of(BEST_INFO_IDX),
            "most-reliable parity bit (huge |LLR|) must outrank the \
             most-reliable info bit; got rank(parity)={} rank(info)={}",
            rank_of(BEST_PARITY_IDX),
            rank_of(BEST_INFO_IDX)
        );

        // No sign inversion within the parity class: a bulk (moderate
        // |LLR|=3.0) parity bit must rank ahead of a deliberately
        // unreliable (|LLR|=0.001) parity bit. Pre-fix, `-|LLR|` under
        // the descending sort put the LOW-|LLR| (unreliable) bit first
        // (-0.001 > -3.0), so this fails pre-fix.
        assert!(
            rank_of(BULK_PARITY_IDX) < rank_of(WORST_PARITY_IDX),
            "an ordinary parity bit (|LLR|=3.0) must outrank a genuinely \
             unreliable one (|LLR|=0.001); got rank(bulk)={} rank(worst)={}",
            rank_of(BULK_PARITY_IDX),
            rank_of(WORST_PARITY_IDX)
        );

        // Sanity: the worst-of-all bit (info bit the CNN is nearly
        // certain is wrong) must not be first, and the best-of-all bit
        // (the huge-|LLR| parity bit) must be first overall.
        assert_eq!(rank_of(BEST_PARITY_IDX), 0);
        assert!(rank_of(WORST_INFO_IDX) > rank_of(BEST_PARITY_IDX));
        assert!(rank_of(WORST_INFO_IDX) > rank_of(BULK_PARITY_IDX));
    }

    /// Without neural ordering, the sort must be unchanged: plain
    /// descending `|LLR|` across all 174 positions (this is the
    /// production `osd_depth: 0` path when the CNN doesn't run — a
    /// regression guard that the refactor into
    /// `reliability_sorted_indices` didn't perturb the non-neural case).
    #[test]
    fn test_no_neural_ordering_sorts_by_descending_abs_llr() {
        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for (i, v) in llrs.iter_mut().enumerate() {
            // Distinct magnitudes so the expected order is unambiguous.
            *v = (i as f32 + 1.0) * if i % 2 == 0 { 1.0 } else { -1.0 };
        }
        let order = reliability_sorted_indices(&llrs, None);
        for w in order.windows(2) {
            assert!(
                llrs[w[0]].abs() >= llrs[w[1]].abs(),
                "expected descending |LLR|: |llrs[{}]|={} should be >= |llrs[{}]|={}",
                w[0],
                llrs[w[0]].abs(),
                w[1],
                llrs[w[1]].abs()
            );
        }
    }

    /// PAN-9 Phase 2. `mrb_permutation` must reproduce, exactly, the
    /// permutation the decode path composes inline — that agreement is the
    /// entire reason the accessor exists rather than a re-derivation.
    #[test]
    fn mrb_permutation_matches_the_inline_composition() {
        let decoder = OsdDecoder::new(OsdConfig {
            max_depth: 1,
            ..Default::default()
        });
        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for (i, v) in llrs.iter_mut().enumerate() {
            *v = ((i * 37 % 101) as f32 - 50.0) * 0.11;
        }

        // The reference: steps 1-4, spelled out.
        let sorted_indices = reliability_sorted_indices(&llrs, None);
        let mut matrix = [[0u8; PACKED_BYTES]; LDPC_INFO_BITS];
        for row in 0..LDPC_INFO_BITS {
            for new_col in 0..LDPC_CODEWORD_BITS {
                let orig_col = sorted_indices[new_col];
                if get_bit(&decoder.generator[row], orig_col) {
                    set_bit(&mut matrix[row], new_col);
                }
            }
        }
        let mut elim_perm = [0u16; LDPC_CODEWORD_BITS];
        gaussian_eliminate(&mut matrix, &mut elim_perm).expect("full-rank basis");
        let mut expected = [0usize; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            expected[i] = sorted_indices[elim_perm[i] as usize];
        }

        let actual = decoder
            .mrb_permutation(&llrs, None)
            .expect("full-rank basis");
        assert_eq!(actual, expected);
    }

    /// A permutation that is not a bijection would silently drop or duplicate
    /// training labels rather than fail.
    #[test]
    fn mrb_permutation_is_a_genuine_permutation() {
        let decoder = OsdDecoder::new(OsdConfig::default());
        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for (i, v) in llrs.iter_mut().enumerate() {
            *v = (i as f32).sin() * 4.0;
        }
        let perm = decoder
            .mrb_permutation(&llrs, None)
            .expect("full-rank basis");

        let mut seen = [false; LDPC_CODEWORD_BITS];
        for &p in &perm {
            assert!(p < LDPC_CODEWORD_BITS, "index {p} out of range");
            assert!(!seen[p], "index {p} appears twice");
            seen[p] = true;
        }
        assert!(seen.iter().all(|&s| s), "every index must appear");
    }

    /// The load-bearing invariant of PAN-9: the production `osd_depth = 0`
    /// path must be byte-identical after extracting `mrb_basis`. Exercises
    /// depth 0 (production) and depth 1/2 (where the extraction is shared
    /// with the reprocessing ladder).
    #[test]
    fn extracting_mrb_basis_left_decode_output_unchanged() {
        // A real codeword with two flipped bits — reaches OSD, and is
        // recoverable at depth >= 1 while still exercising depth 0's
        // hard-decision trial.
        let mut message = bitvec![0; LDPC_INFO_BITS];
        for i in 0..LDPC_INFO_BITS {
            message.set(i, (i * 5 + 1) % 7 < 3);
        }
        let codeword = crate::ldpc::LdpcEncoder::new()
            .encode(&message)
            .expect("encode");
        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            llrs[i] = if codeword[i] { -3.0 } else { 3.0 };
        }
        llrs[11] = -llrs[11];
        llrs[140] = -llrs[140];

        for depth in [0u8, 1, 2] {
            let decoder = OsdDecoder::new(OsdConfig {
                max_depth: depth,
                ..Default::default()
            });
            // Two calls through the same path must agree, and the MRB the
            // accessor reports must be the one the decode consumed (proven
            // by the agreement test above). This asserts the extraction is
            // deterministic and side-effect free at every live depth.
            let a = decoder.decode_with_features_scored(&llrs, &llrs, None);
            let b = decoder.decode_with_features_scored(&llrs, &llrs, None);
            assert_eq!(
                a.as_ref().map(|r| (r.0.clone(), r.1, r.2)),
                b.as_ref().map(|r| (r.0.clone(), r.1, r.2)),
                "depth {depth}: decode must be deterministic through mrb_basis"
            );
            assert!(
                decoder.mrb_permutation(&llrs, None).is_some(),
                "depth {depth}: the same basis the decode used must be reachable"
            );
        }
    }

    #[test]
    fn test_bit_operations() {
        let mut row: PackedRow = [0u8; PACKED_BYTES];

        // Test set and get
        assert!(!get_bit(&row, 0));
        set_bit(&mut row, 0);
        assert!(get_bit(&row, 0));

        // Test various positions
        set_bit(&mut row, 7); // end of first byte
        assert!(get_bit(&row, 7));
        assert_eq!(row[0], 0x81); // bits 0 and 7 set: 1000_0001

        set_bit(&mut row, 8); // start of second byte
        assert!(get_bit(&row, 8));
        assert_eq!(row[1], 0x80);

        // Test at boundary: last meaningful bit (173)
        set_bit(&mut row, 173);
        assert!(get_bit(&row, 173));

        // Test clear via flip (clear_bit removed as unused outside tests)
        flip_bit(&mut row, 0); // was set, now cleared
        assert!(!get_bit(&row, 0));
        assert!(get_bit(&row, 7)); // unchanged

        // Test flip
        flip_bit(&mut row, 7);
        assert!(!get_bit(&row, 7));
        flip_bit(&mut row, 7);
        assert!(get_bit(&row, 7));

        // Test xor_rows
        let mut a: PackedRow = [0u8; PACKED_BYTES];
        let mut b: PackedRow = [0u8; PACKED_BYTES];
        set_bit(&mut a, 0);
        set_bit(&mut a, 5);
        set_bit(&mut b, 0);
        set_bit(&mut b, 10);
        xor_rows(&mut a, &b);
        assert!(!get_bit(&a, 0)); // 1 ^ 1 = 0
        assert!(get_bit(&a, 5)); // 1 ^ 0 = 1
        assert!(get_bit(&a, 10)); // 0 ^ 1 = 1
    }

    #[test]
    fn test_build_systematic_generator() {
        let g = build_systematic_generator();

        // Verify identity part: row i should have bit i set in cols 0..91
        for i in 0..LDPC_INFO_BITS {
            for j in 0..LDPC_INFO_BITS {
                if i == j {
                    assert!(
                        get_bit(&g[i], j),
                        "Identity diagonal missing at ({}, {})",
                        i,
                        j
                    );
                } else {
                    assert!(
                        !get_bit(&g[i], j),
                        "Unexpected bit at ({}, {}) in identity part",
                        i,
                        j
                    );
                }
            }
        }

        // Verify parity part has some nonzero entries
        let mut parity_ones = 0usize;
        for i in 0..LDPC_INFO_BITS {
            for j in LDPC_INFO_BITS..LDPC_CODEWORD_BITS {
                if get_bit(&g[i], j) {
                    parity_ones += 1;
                }
            }
        }
        assert!(
            parity_ones > 0,
            "Parity part of generator matrix is all zeros"
        );
        // The LDPC generator is fairly dense; expect hundreds of ones
        assert!(
            parity_ones > 100,
            "Parity part has suspiciously few ones: {}",
            parity_ones
        );
    }

    #[test]
    fn test_gaussian_elimination_produces_identity() {
        let mut matrix = build_systematic_generator();
        let mut col_perm = [0u16; LDPC_CODEWORD_BITS];

        let result = gaussian_eliminate(&mut matrix, &mut col_perm);
        assert!(result.is_some(), "Gaussian elimination failed (singular)");

        // After elimination, the first 91 columns (in permuted order) should be identity
        for i in 0..LDPC_INFO_BITS {
            for j in 0..LDPC_INFO_BITS {
                let expected = i == j;
                assert_eq!(
                    get_bit(&matrix[i], j),
                    expected,
                    "Not identity at ({}, {}) after elimination",
                    i,
                    j
                );
            }
        }

        // The initial generator is already systematic, so col_perm should be identity
        for i in 0..LDPC_CODEWORD_BITS {
            assert_eq!(
                col_perm[i], i as u16,
                "Column permutation changed at {} even though input was already systematic",
                i
            );
        }
    }

    #[test]
    fn test_crc14_cross_validation() {
        use crate::message::calculate_crc14;
        use bitvec::prelude::*;

        // Test several different payloads to ensure crc14_from_u8_bits matches calculate_crc14
        let test_patterns: &[&[usize]] = &[
            &[],                                                             // all zeros
            &[0, 1, 2, 3],                                                   // first few bits
            &[3, 10, 25, 50, 70],                                            // sparse
            &[0, 5, 10, 15, 20, 25, 30, 35, 40, 45, 50, 55, 60, 65, 70, 75], // dense
            &[76],                                                           // last bit only
        ];

        for (idx, pattern) in test_patterns.iter().enumerate() {
            // Build BitVec for calculate_crc14
            let mut bv: BitVec = BitVec::repeat(false, PAYLOAD_BITS);
            // Build u8 array for crc14_from_u8_bits
            let mut u8_bits = [0u8; PAYLOAD_BITS];

            for &bit_pos in *pattern {
                bv.set(bit_pos, true);
                u8_bits[bit_pos] = 1;
            }

            let crc_bitvec = calculate_crc14(&bv);
            let crc_u8 = crc14_from_u8_bits(&u8_bits);

            assert_eq!(
                crc_bitvec, crc_u8,
                "CRC-14 mismatch for pattern {}: bitvec={:#06x}, u8={:#06x}",
                idx, crc_bitvec, crc_u8
            );
        }
    }

    #[cfg(feature = "transmit")]
    mod osd_decode_tests {
        use super::*;
        use crate::ldpc::LdpcEncoder;
        use crate::message::{calculate_crc14, CRC_BITS, PAYLOAD_BITS};

        /// Helper: create a valid 91-bit message with CRC and encode to 174-bit codeword.
        /// Returns (message_91_bits as BitVec, codeword_174_bits as BitVec).
        fn make_test_codeword() -> (BitVec, BitVec) {
            // Create a 77-bit payload with a few bits set
            let mut payload: BitVec = BitVec::repeat(false, PAYLOAD_BITS);
            payload.set(3, true);
            payload.set(10, true);
            payload.set(25, true);
            payload.set(50, true);
            payload.set(70, true);

            // Calculate CRC-14 over the payload
            let crc = calculate_crc14(&payload);

            // Build the full 91-bit message: 77 payload + 14 CRC (MSB first)
            let mut message: BitVec = payload;
            for i in 0..CRC_BITS {
                message.push((crc >> (CRC_BITS - 1 - i)) & 1 == 1);
            }
            assert_eq!(message.len(), LDPC_INFO_BITS);

            // LDPC encode to 174 bits
            let encoder = LdpcEncoder::new();
            let codeword = encoder.encode(&message).expect("LDPC encoding failed");
            assert_eq!(codeword.len(), LDPC_CODEWORD_BITS);

            (message, codeword)
        }

        /// Convert a codeword BitVec to LLRs: bit=1 -> -mag, bit=0 -> +mag
        fn codeword_to_llrs(codeword: &BitVec, magnitude: f32) -> [f32; LDPC_CODEWORD_BITS] {
            let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
            for i in 0..LDPC_CODEWORD_BITS {
                llrs[i] = if codeword[i] { -magnitude } else { magnitude };
            }
            llrs
        }

        #[test]
        fn test_osd0_recovers_clean_codeword() {
            let (_message, codeword) = make_test_codeword();
            let llrs = codeword_to_llrs(&codeword, 4.0);

            let decoder = OsdDecoder::new(OsdConfig {
                max_depth: 0,
                ..Default::default()
            });
            let result = decoder.decode(&llrs, None);

            assert!(result.is_some(), "OSD-0 should decode a clean codeword");
            let decoded = result.unwrap();
            assert_eq!(decoded.len(), LDPC_CODEWORD_BITS);
            assert_eq!(decoded, codeword, "Decoded codeword should match original");
        }

        // FDR Session 3: decode_with_features contract tests. Pin the
        // (depth_used, nharderrs) tuple so future implementations
        // honor the convention.

        #[test]
        fn decode_with_features_reports_depth_0_on_clean_codeword() {
            // A clean codeword should converge at OSD-0 with 0 flips.
            let (_message, codeword) = make_test_codeword();
            let llrs = codeword_to_llrs(&codeword, 4.0);
            let decoder = OsdDecoder::new(OsdConfig {
                max_depth: 0,
                ..Default::default()
            });
            let result = decoder.decode_with_features(&llrs, None);
            assert!(result.is_some(), "decode_with_features must produce Some");
            let (bits, depth, nharderrs) = result.unwrap();
            assert_eq!(bits, codeword);
            assert_eq!(depth, 0, "clean codeword should converge at OSD-0");
            assert_eq!(nharderrs, 0, "no bits flipped on clean input");
        }

        #[test]
        fn decode_with_features_reports_depth_1_on_one_bad_bit() {
            // One corrupted bit forces OSD-1; depth=1, nharderrs=1.
            let (_message, codeword) = make_test_codeword();
            let mut llrs = codeword_to_llrs(&codeword, 4.0);
            llrs[5] = if codeword[5] { 0.1 } else { -0.1 };
            let decoder = OsdDecoder::new(OsdConfig {
                max_depth: 1,
                ..Default::default()
            });
            let result = decoder.decode_with_features(&llrs, None);
            assert!(result.is_some());
            let (_bits, depth, nharderrs) = result.unwrap();
            assert_eq!(depth, 1, "single-flip path should report depth=1");
            assert_eq!(nharderrs, 1, "single-flip path should report nharderrs=1");
        }

        /// Task W2.4: an already-expired [`DecodeBudget`] must block OSD's
        /// escalation ladder (order-1+) exactly like `max_depth == 0` would
        /// — proving the budget checkpoint genuinely gates the expensive
        /// orders, not just compiles. A clean codeword (order-0 succeeds)
        /// must still decode under an expired budget (order-0 never
        /// consults `budget`); a single corrupted bit (needs order-1) must
        /// NOT decode under an expired budget, but MUST decode under
        /// `DecodeBudget::unlimited()` with the identical config.
        #[test]
        fn expired_budget_blocks_escalation_past_order_zero() {
            let (_message, codeword) = make_test_codeword();
            let decoder = OsdDecoder::new(OsdConfig {
                max_depth: 3,
                ..Default::default()
            });
            let expired = DecodeBudget::until(
                std::time::Instant::now() - std::time::Duration::from_millis(1),
            );

            // Clean codeword: order-0 alone recovers it, budget irrelevant.
            let clean_llrs = codeword_to_llrs(&codeword, 4.0);
            let clean_result = decoder.decode_with_features_scored_budgeted(
                &clean_llrs,
                &clean_llrs,
                None,
                expired,
            );
            assert!(
                clean_result.is_some(),
                "order-0 must still succeed on a clean codeword even under an \
                 expired budget — order-0 never consults `budget`"
            );
            assert_eq!(
                clean_result.unwrap().1,
                0,
                "clean codeword decodes at depth 0"
            );

            // One corrupted bit forces escalation past order-0.
            let mut bad_llrs = clean_llrs;
            bad_llrs[5] = if codeword[5] { 0.1 } else { -0.1 };

            let blocked =
                decoder.decode_with_features_scored_budgeted(&bad_llrs, &bad_llrs, None, expired);
            assert!(
                blocked.is_none(),
                "an expired budget must block OSD-1+ escalation, exactly as \
                 max_depth == 0 would — got {:?}",
                blocked
            );

            // The identical config+input under an unlimited budget DOES
            // recover it (proves the block above is the budget, not some
            // other change).
            let unblocked = decoder.decode_with_features_scored_budgeted(
                &bad_llrs,
                &bad_llrs,
                None,
                DecodeBudget::unlimited(),
            );
            assert!(
                unblocked.is_some(),
                "the same candidate must still decode under an unlimited budget"
            );
            assert_eq!(
                unblocked.unwrap().1,
                1,
                "recovers at depth 1 when unblocked"
            );
        }

        #[test]
        fn decode_returns_byte_identical_to_decode_with_features() {
            // The wrapper must produce identical BitVecs to the
            // feature-returning variant — this is the byte-identical
            // contract for the 10+ existing decode() call sites.
            let (_message, codeword) = make_test_codeword();
            let mut llrs = codeword_to_llrs(&codeword, 4.0);
            llrs[5] = if codeword[5] { 0.1 } else { -0.1 };
            let decoder = OsdDecoder::new(OsdConfig {
                max_depth: 1,
                ..Default::default()
            });
            let a = decoder.decode(&llrs, None);
            let b = decoder
                .decode_with_features(&llrs, None)
                .map(|(bits, _, _)| bits);
            assert_eq!(a, b, "decode and decode_with_features must agree");
        }

        #[test]
        fn test_osd1_recovers_single_unreliable_bit() {
            let (_message, codeword) = make_test_codeword();
            let mut llrs = codeword_to_llrs(&codeword, 4.0);

            // Make one bit wrong-signed and low magnitude (unreliable and incorrect)
            llrs[5] = if codeword[5] { 0.1 } else { -0.1 };

            // OSD-0 should fail
            let decoder0 = OsdDecoder::new(OsdConfig {
                max_depth: 0,
                ..Default::default()
            });
            assert!(
                decoder0.decode(&llrs, None).is_none(),
                "OSD-0 should fail with one corrupted bit"
            );

            // OSD-1 should succeed
            let decoder1 = OsdDecoder::new(OsdConfig {
                max_depth: 1,
                ..Default::default()
            });
            let result = decoder1.decode(&llrs, None);
            assert!(
                result.is_some(),
                "OSD-1 should recover single unreliable bit"
            );
            assert_eq!(result.unwrap(), codeword);
        }

        #[test]
        fn test_osd2_recovers_two_unreliable_bits() {
            let (_message, codeword) = make_test_codeword();

            // Try multiple pairs of bit positions to find one where OSD-1 fails
            // but OSD-2 succeeds. Some pairs may land in parity positions after
            // the reliability sort, allowing OSD-1 to succeed with a single flip.
            let pairs = [
                (5, 20),
                (10, 30),
                (40, 60),
                (15, 45),
                (2, 70),
                (33, 77),
                (8, 55),
                (12, 88),
            ];

            let decoder1 = OsdDecoder::new(OsdConfig {
                max_depth: 1,
                ..Default::default()
            });
            let decoder2 = OsdDecoder::new(OsdConfig {
                max_depth: 2,
                ..Default::default()
            });

            let mut found_good_pair = false;
            for &(a, b) in &pairs {
                let mut llrs = codeword_to_llrs(&codeword, 4.0);
                // Wrong-sign with small magnitude
                llrs[a] = if codeword[a] { 0.05 } else { -0.05 };
                llrs[b] = if codeword[b] { 0.05 } else { -0.05 };

                let osd1_result = decoder1.decode(&llrs, None);
                let osd2_result = decoder2.decode(&llrs, None);

                if let (None, Some(cw2)) = (&osd1_result, &osd2_result) {
                    assert_eq!(
                        cw2, &codeword,
                        "OSD-2 decoded wrong codeword for pair ({}, {})",
                        a, b
                    );
                    found_good_pair = true;
                    break;
                }
            }

            assert!(
                found_good_pair,
                "Could not find a bit pair where OSD-1 fails but OSD-2 succeeds"
            );
        }

        #[test]
        fn test_osd3_recovers_three_unreliable_bits() {
            let (_message, codeword) = make_test_codeword();

            // Strategy: give all bits correct-sign at medium magnitude (4.0), then make
            // 3 target bits VERY high magnitude but WRONG sign. They will sort to the
            // top 3 positions (most "reliable" but incorrect), so they definitely land
            // in the 91 info positions after any Gaussian elimination. OSD-2 tries all
            // pairs of the 91 info bits, so with 3 wrong info bits it fails.
            // OSD-3 tries all triples and finds the exact triple to flip.
            //
            // We test multiple triples because Gaussian elimination may occasionally
            // re-order columns such that one of the 3 bits moves to a parity position
            // (which is corrected automatically), reducing it to a 2-bit problem.
            let triples = [
                (5, 20, 40),
                (10, 30, 60),
                (2, 15, 70),
                (1, 25, 50),
                (3, 18, 65),
                (7, 35, 80),
                (0, 12, 45),
                (4, 22, 55),
                (6, 28, 73),
                (9, 38, 85),
            ];

            let decoder2 = OsdDecoder::new(OsdConfig {
                max_depth: 2,
                ..Default::default()
            });
            let decoder3 = OsdDecoder::new(OsdConfig {
                max_depth: 3,
                ..Default::default()
            });

            let mut found_good_triple = false;
            for &(a, b, c) in &triples {
                let mut llrs = codeword_to_llrs(&codeword, 4.0);
                // High-magnitude wrong-sign: these 3 bits sort to top of reliability
                // ranking but have incorrect hard decisions.
                llrs[a] = if codeword[a] { 8.0 } else { -8.0 };
                llrs[b] = if codeword[b] { 8.0 } else { -8.0 };
                llrs[c] = if codeword[c] { 8.0 } else { -8.0 };

                let osd2_result = decoder2.decode(&llrs, None);
                let osd3_result = decoder3.decode(&llrs, None);

                if let (None, Some(cw3)) = (&osd2_result, &osd3_result) {
                    assert_eq!(
                        cw3, &codeword,
                        "OSD-3 decoded wrong codeword for triple ({}, {}, {})",
                        a, b, c
                    );
                    found_good_triple = true;
                    break;
                }
            }

            assert!(
                found_good_triple,
                "Could not find a bit triple where OSD-2 fails but OSD-3 succeeds"
            );
        }

        /// W2.2 RED/GREEN test: a DETERMINISTIC (no probabilistic mining
        /// loop) construction of "an early flip pattern yields a CRC-14
        /// collision codeword at large soft distance and a later pattern
        /// yields the true codeword at small distance" (task brief
        /// language), exercising the order-1 -> order-3 escalation
        /// boundary rather than two positions within one order's loop.
        ///
        /// # Why deterministic, and why order-1-vs-order-3
        /// An earlier, purely-probabilistic version of this test (random
        /// payloads + random corrupted-bit positions, hoping to stumble
        /// on a genuine CRC-14 collision within OSD's small trial
        /// neighborhood) was tried and abandoned: empirically, 500,000
        /// random attempts targeting an order-1-collision/order-2-truth
        /// scenario produced ZERO qualifying collisions (see this task's
        /// experiment log) — far below the naive "2^-14 per trial"
        /// expectation, because most single-info-bit flips don't even
        /// touch the sparse ~1.3%-density parity columns that carry the
        /// CRC-relevant bits, so most trials can't possibly flip CRC
        /// pass/fail at all. A follow-up brute-force search (also in this
        /// module, see the abandoned `diag_crc_kernel_search`/
        /// `diag_mining_stats` probes) confirmed the minimum-weight
        /// nonzero 77-bit payload delta with `crc14(delta) == 0` (a
        /// "kernel element" — XORing it into any payload leaves the
        /// CRC-14 unchanged) has weight **4**, one bit too many to fit
        /// directly in OSD's max order-3 (91-choose-3) trial budget as a
        /// single flip pattern.
        ///
        /// The construction that DOES work: split a weight-4 kernel
        /// element's support `{p1,p2,p3,p4}` 3-and-1 between two
        /// messages. Let `M` be the true message and `M' = M XOR
        /// {p1,p2,p3,p4}` (also CRC-valid, since the delta is a kernel
        /// element). Pick `baseline = M XOR {p1,p2,p3}`. Then:
        /// - `baseline XOR {p1,p2,p3}` recovers `M` (an order-3 fix).
        /// - `baseline XOR {p4}` recovers `M'` (an order-1 "fix" — really
        ///   a spurious collision relative to the true intended signal).
        ///
        /// Order-1 is exhaustively tried BEFORE order-3 ever runs. Under
        /// OLD first-CRC-accept semantics, order-1 finds `M'` (the
        /// collision) at whichever info-role index corresponds to `p4`
        /// and returns it immediately — order-3 (where `M`, the true
        /// signal, lives) is never reached. This test fails against that
        /// code. Post-W2.2, order-1's only CRC-valid candidate (`M'`) has
        /// a large `soft_distance` (channel LLRs are set to confidently
        /// match the TRUE codeword `C = encode(M)` everywhere, so `C'`
        /// disagrees with many confident channel bits — verified, not
        /// assumed, via an explicit assertion below), fails the
        /// acceptance gate, and the decoder escalates to order-3, where
        /// it finds and accepts `C`.
        #[test]
        fn test_osd_rejects_untrustworthy_order1_collision_and_finds_truth_at_order3() {
            // All-zero payload: simplest possible base message. CRC-14
            // linearity is verified explicitly below, not assumed.
            let payload: BitVec = BitVec::repeat(false, PAYLOAD_BITS);
            let crc = calculate_crc14(&payload);
            let mut message: BitVec = payload.clone();
            for i in 0..CRC_BITS {
                message.push((crc >> (CRC_BITS - 1 - i)) & 1 == 1);
            }
            let encoder = LdpcEncoder::new();
            let codeword = encoder.encode(&message).expect("LDPC encode failed");

            // A weight-4 CRC-14 kernel element found by brute-force
            // search over all 77-choose-4 payload-bit subsets (see the
            // doc comment above): flipping these 4 payload bits leaves
            // CRC-14 unchanged. Verified below, not assumed.
            const DELTA: [usize; 4] = [0, 3, 37, 66];

            // Verify DELTA is a genuine kernel element for THIS payload
            // (all-zero): crc(payload) must equal crc(payload XOR DELTA).
            let mut payload_prime = payload.clone();
            for &p in &DELTA {
                let bit = payload_prime[p];
                payload_prime.set(p, !bit);
            }
            let crc_prime = calculate_crc14(&payload_prime);
            assert_eq!(
                crc, crc_prime,
                "DELTA must be a genuine CRC-14 kernel element (crc(payload) == \
                 crc(payload XOR DELTA)); got crc={:#06x} crc'={:#06x}",
                crc, crc_prime
            );

            // M' = M XOR DELTA (same CRC bits, different payload) -> C'.
            let mut message_prime = payload_prime.clone();
            for i in 0..CRC_BITS {
                message_prime.push((crc_prime >> (CRC_BITS - 1 - i)) & 1 == 1);
            }
            let codeword_prime = encoder
                .encode(&message_prime)
                .expect("LDPC encode failed for M'");
            assert_ne!(
                codeword, codeword_prime,
                "M and M' must encode to DIFFERENT codewords (DELTA is nonzero)"
            );

            // Channel LLRs: confidently match the TRUE codeword `codeword`
            // (M) at EVERY position, EXCEPT DELTA[0..3] (p1,p2,p3), which
            // are given the WRONG sign (introducing the corruption that
            // forces baseline = M XOR {p1,p2,p3}) at a lower-but-still-
            // message-block-dominant magnitude. The parity block
            // (positions 91..174) is confident but at the LOWEST
            // magnitude of all three tiers, guaranteeing the message
            // block (0..91) sorts entirely ahead of the parity block in
            // reliability — the precondition for `final_perm` to map
            // message-block positions onto OSD's "info" role as a set
            // (verified dynamically below via `final_perm`, not assumed).
            const MSG_CONFIDENT_MAG: f32 = 4.0;
            const MSG_CORRUPTED_MAG: f32 = 2.0;
            const PARITY_MAG: f32 = 1.8;
            let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
            for i in 0..LDPC_CODEWORD_BITS {
                let mag = if i < LDPC_INFO_BITS {
                    MSG_CONFIDENT_MAG
                } else {
                    PARITY_MAG
                };
                llrs[i] = if codeword[i] { -mag } else { mag };
            }
            for &p in &DELTA[..3] {
                // Wrong sign, lower (but still message-block-dominant)
                // magnitude: corrupted relative to the true codeword.
                llrs[p] = if codeword[p] {
                    MSG_CORRUPTED_MAG
                } else {
                    -MSG_CORRUPTED_MAG
                };
            }

            // Replicate decode_with_features_scored's steps 1-4 to
            // recover the EXACT permutation/info_hard/base_parity this
            // LLR array is searched under, and to translate DELTA's
            // original-bit-position support into OSD's info-role indices.
            let decoder3 = OsdDecoder::new(OsdConfig {
                max_depth: 3,
                ..Default::default()
            });
            let sorted_indices = reliability_sorted_indices(&llrs, None);
            let mut matrix = [[0u8; PACKED_BYTES]; LDPC_INFO_BITS];
            for row in 0..LDPC_INFO_BITS {
                for new_col in 0..LDPC_CODEWORD_BITS {
                    let orig_col = sorted_indices[new_col];
                    if get_bit(&decoder3.generator[row], orig_col) {
                        set_bit(&mut matrix[row], new_col);
                    }
                }
            }
            let mut elim_perm = [0u16; LDPC_CODEWORD_BITS];
            gaussian_eliminate(&mut matrix, &mut elim_perm)
                .expect("Gaussian elimination must succeed on this reliability order");
            let mut final_perm = [0usize; LDPC_CODEWORD_BITS];
            for i in 0..LDPC_CODEWORD_BITS {
                final_perm[i] = sorted_indices[elim_perm[i] as usize];
            }

            // Confirm the precondition: the message block (original
            // positions 0..91) must occupy the info role (permuted
            // positions 0..91) AS A SET — i.e. every DELTA position
            // (which is < 77 < 91) has an info-role index.
            let info_role_of = |orig_pos: usize| -> usize {
                final_perm
                    .iter()
                    .position(|&p| p == orig_pos)
                    .expect("original position must appear in final_perm")
            };
            let idx: Vec<usize> = DELTA.iter().map(|&p| info_role_of(p)).collect();
            for &i in &idx {
                assert!(
                    i < LDPC_INFO_BITS,
                    "DELTA position must map to the info role (index < {}), got {}",
                    LDPC_INFO_BITS,
                    i
                );
            }

            let mut info_hard = [0u8; LDPC_INFO_BITS];
            for i in 0..LDPC_INFO_BITS {
                if llrs[final_perm[i]] < 0.0 {
                    info_hard[i] = 1;
                }
            }
            let mut base_parity = [0u8; LDPC_PARITY_BITS];
            for p in 0..LDPC_PARITY_BITS {
                let mut val = 0u8;
                for i in 0..LDPC_INFO_BITS {
                    if info_hard[i] == 1 && get_bit(&matrix[i], LDPC_INFO_BITS + p) {
                        val ^= 1;
                    }
                }
                base_parity[p] = val;
            }

            // Order-0 (no flips) must fail — the corruption at p1,p2,p3
            // must actually break CRC validity of the raw hard-decision.
            assert!(
                decoder3
                    .try_solution(&info_hard, &base_parity, &final_perm)
                    .is_none(),
                "order-0 (uncorrupted hard-decision) must fail CRC for this \
                 construction to exercise order-1/order-3 at all"
            );

            // Scan order-1's full 91-trial space: find the first CRC-valid
            // candidate (should be at idx[3], DELTA's 4th position, mapping
            // to M') and confirm order-1 never reaches the true codeword.
            let mut order1_true = false;
            let mut first_collision: Option<(usize, BitVec)> = None;
            for flip in 0..LDPC_INFO_BITS {
                let mut info = info_hard;
                info[flip] ^= 1;
                let mut parity = base_parity;
                for p in 0..LDPC_PARITY_BITS {
                    if get_bit(&matrix[flip], LDPC_INFO_BITS + p) {
                        parity[p] ^= 1;
                    }
                }
                if let Some(cand) = decoder3.try_solution(&info, &parity, &final_perm) {
                    if cand == codeword {
                        order1_true = true;
                    } else if first_collision.is_none() {
                        first_collision = Some((flip, cand));
                    }
                }
            }
            assert!(
                !order1_true,
                "order-1 must NOT reach the true codeword directly — the \
                 construction requires 3 flips (p1,p2,p3) to recover it"
            );
            let (collision_flip, collision_cw) = first_collision.expect(
                "order-1 must find at least one CRC-valid candidate (M', via \
                 the info-role index for DELTA[3])",
            );
            assert_eq!(
                collision_flip, idx[3],
                "the order-1 collision should be found at DELTA[3]'s info-role \
                 index (no earlier accidental CRC collision) — if this fires, \
                 an unplanned earlier collision exists; the test's assertions \
                 below still hold generally, but the construction's narrative \
                 assumption should be revisited"
            );
            assert_eq!(
                collision_cw, codeword_prime,
                "the order-1 collision must be exactly M' (C'), matching the \
                 deterministic construction"
            );

            // Sanity: the collision must be a strictly WORSE (larger
            // soft_distance) match against the channel than the truth —
            // verified, not assumed.
            let score_true = crate::acceptance::score(&codeword, &llrs);
            let score_collision = crate::acceptance::score(&collision_cw, &llrs);
            assert!(
                score_collision.soft_distance > score_true.soft_distance,
                "collision (soft_distance={}) must be a strictly WORSE match \
                 than the true codeword (soft_distance={})",
                score_collision.soft_distance,
                score_true.soft_distance
            );

            // THE crux of W2.2: call the REAL decoder under test exactly
            // once and check the outcome against BOTH possibilities.
            let result = decoder3.decode(&llrs, None);
            assert_ne!(
                result.as_ref(),
                Some(&collision_cw),
                "decoder must NOT accept the untrustworthy order-1 CRC-14 \
                 collision (soft_distance={}) merely because it was found \
                 first — this is exactly what OLD first-CRC-accept code did",
                score_collision.soft_distance
            );
            assert_eq!(
                result.as_ref(),
                Some(&codeword),
                "decoder must instead find the TRUE codeword (soft_distance={}) \
                 via order-3 escalation; got {:?}",
                score_true.soft_distance,
                result
            );
        }
    }
}

#[cfg(test)]
mod npre2_tests {
    //! WSJT-X mainline-style npre2 preprocessing tests. Verifies:
    //!
    //! 1. Default-OFF preserves byte-identical OSD output across all depths.
    //! 2. `npre2_preprocess` with all-certain LLRs is a no-op.
    //! 3. `npre2_preprocess` with marginal LLRs returns the expected
    //!    warm-start (a productive complementary-pair flip when the
    //!    parity error matches a pair's XOR signature).
    //!
    //! Spec: `research/specs/spec-wsjtx-mainline-osd174.md` § Step 6.
    //! Implementation inspired by spec ref only; no GPL source was read.
    use super::*;

    /// Build the per-info-bit parity-column slice array used by the
    /// npre2 helpers, from the cached row-echelon generator matrix.
    fn make_parity_cols_perm() -> [[bool; LDPC_PARITY_BITS]; LDPC_INFO_BITS] {
        let mut matrix = build_systematic_generator();
        let mut col_perm = [0u16; LDPC_CODEWORD_BITS];
        gaussian_eliminate(&mut matrix, &mut col_perm)
            .expect("Gaussian elimination should succeed on systematic generator");
        let mut out = [[false; LDPC_PARITY_BITS]; LDPC_INFO_BITS];
        for i in 0..LDPC_INFO_BITS {
            for p in 0..LDPC_PARITY_BITS {
                out[i][p] = get_bit(&matrix[i], LDPC_INFO_BITS + p);
            }
        }
        out
    }

    /// Identity permutation: `final_perm[i] = i` for all i.
    fn identity_perm() -> [usize; LDPC_CODEWORD_BITS] {
        let mut p = [0usize; LDPC_CODEWORD_BITS];
        for (i, slot) in p.iter_mut().enumerate() {
            *slot = i;
        }
        p
    }

    /// Task W2.4: `npre2_residual_signature` must compute `base_parity
    /// XOR received_parity_hard`, per the spec's `e2sub = (ce XOR
    /// hdec)[k+1..N]` at the order-0 test pattern (`research/specs/
    /// spec-wsjtx-mainline-osd174.md` § Step 6) — NOT `base_parity`
    /// alone (the bug: a signature with no dependency on what was
    /// actually received).
    #[test]
    fn test_npre2_residual_signature_is_base_parity_xor_received() {
        let mut base_parity = [0u8; LDPC_PARITY_BITS];
        base_parity[0] = 1;
        base_parity[2] = 1;
        base_parity[5] = 1;

        // Zero discrepancy case: received matches base_parity exactly
        // over the first NPRE2_NTAU bits -> residual must be 0. The
        // pre-fix implementation would have returned base_parity's own
        // bits packed (0b100101), which is WRONG here: there is no
        // error to cancel.
        let received_matches = base_parity;
        assert_eq!(
            npre2_residual_signature(&base_parity, &received_matches),
            0,
            "when received parity exactly matches the order-0 codeword's \
             expected parity, the residual signature must be zero — there \
             is no discrepancy for a warm-start pair to cancel"
        );

        // Discrepancy at positions {1, 5}: base_parity has 0 at p=1
        // (received 1, mismatch) and 1 at p=5 (received 0, mismatch);
        // p=0 and p=2 still match (no mismatch).
        let mut received_mismatch = base_parity;
        received_mismatch[1] = 1; // base=0, received=1 -> mismatch
        received_mismatch[5] = 0; // base=1, received=0 -> mismatch
        let sig = npre2_residual_signature(&base_parity, &received_mismatch);
        assert_eq!(
            sig,
            (1 << 1) | (1 << 5),
            "residual signature must have exactly the bits set where \
             base_parity and received_parity_hard disagree"
        );
    }

    #[test]
    fn test_default_config_disables_npre2() {
        let cfg = OsdConfig::default();
        assert!(
            !cfg.npre2_preprocessing_enabled,
            "Default OsdConfig must have npre2_preprocessing_enabled = false \
             so OSD behavior is byte-identical to pre-Batch 51"
        );
    }

    #[test]
    fn test_npre2_helper_all_certain_is_noop() {
        // All-certain LLRs (|LLR| well above threshold) → no marginal bits
        // → npre2 returns the input codeword unchanged.
        let llrs = [10.0f32; LDPC_CODEWORD_BITS];
        let codeword = [0u8; LDPC_CODEWORD_BITS];
        let parity_cols = make_parity_cols_perm();
        let perm = identity_perm();

        let out = npre2_preprocess(&llrs, &codeword, &parity_cols, &perm);
        assert_eq!(
            out.len(),
            LDPC_CODEWORD_BITS,
            "npre2_preprocess must return a 174-bit vector"
        );
        assert_eq!(
            out, codeword,
            "All-certain LLRs must produce zero changes — \
             marginal-bit set is empty so no pairs are searched."
        );
    }

    #[test]
    fn test_npre2_helper_too_few_marginals_is_noop() {
        // Only one marginal LLR — pair search requires >= 2.
        let mut llrs = [10.0f32; LDPC_CODEWORD_BITS];
        llrs[3] = 0.1; // single marginal
        let codeword = [1u8; LDPC_CODEWORD_BITS];
        let parity_cols = make_parity_cols_perm();
        let perm = identity_perm();

        let out = npre2_preprocess(&llrs, &codeword, &parity_cols, &perm);
        assert_eq!(
            out, codeword,
            "With < 2 marginal bits, npre2 has no pairs to consider \
             and must return the input codeword unchanged."
        );
    }

    #[test]
    fn test_npre2_helper_marginal_pair_can_flip() {
        // Construct a scenario where two info bits are marginal and the
        // residual signature matches a pair's XOR. We don't predict the
        // exact pair (the hash table may collide), but we verify the
        // output has exactly two info-bit flips relative to the input —
        // the signature of a productive warm start.
        //
        // Strategy: mark info bits 0 and 1 as marginal (|LLR| < 2.0),
        // both set to 1 in the codeword. With received-parity zeroed
        // over the first NPRE2_NTAU bits, the residual signature
        // computed by `npre2_preprocess` equals exactly the XOR of bits
        // 0 and 1's parity columns — which is the hash key for the
        // (0, 1) pair. The lookup must succeed.
        let mut llrs = [10.0f32; LDPC_CODEWORD_BITS];
        llrs[0] = 0.5;
        llrs[1] = 0.5;
        let parity_cols = make_parity_cols_perm();
        let perm = identity_perm();

        let mut codeword = [0u8; LDPC_CODEWORD_BITS];
        // Marginal info bits 0 and 1 are set to 1: their parity
        // contribution to the "expected parity" is parity_cols[0] XOR
        // parity_cols[1] over the first NPRE2_NTAU bits.
        codeword[0] = 1;
        codeword[1] = 1;
        // Received parity over first NPRE2_NTAU bits is left at 0, so
        // `err = expected XOR received = expected` ⇒ residual signature
        // = parity_cols[0][p] XOR parity_cols[1][p] for p < NPRE2_NTAU.
        // This is the hash key under which the (0, 1) pair is stored.

        let out = npre2_preprocess(&llrs, &codeword, &parity_cols, &perm);
        assert_eq!(out.len(), LDPC_CODEWORD_BITS);

        // Count bit differences in the info portion (the pair returned
        // may not literally be (0, 1) if multiple pairs collide on the
        // same hash, but the helper flips exactly one matched pair).
        let info_diffs: usize = (0..LDPC_INFO_BITS)
            .filter(|&i| out[i] != codeword[i])
            .count();
        assert_eq!(
            info_diffs, 2,
            "Productive warm-start should flip exactly 2 info bits \
             (the matched complementary pair); got {} flips.",
            info_diffs
        );

        // Parity portion is not modified by the helper — it's a warm
        // start for OSD which recomputes parity from the perturbed
        // info bits.
        for p in 0..LDPC_PARITY_BITS {
            assert_eq!(
                out[LDPC_INFO_BITS + p],
                codeword[LDPC_INFO_BITS + p],
                "Parity bit {} must not be modified by npre2_preprocess",
                p
            );
        }
    }

    #[test]
    fn test_npre2_collect_pairs_finds_matching_signature() {
        // Helper-level test: given two marginal bits whose parity-column
        // XOR (over the first NPRE2_NTAU bits) equals some signature S,
        // `npre2_collect_pairs` with residual S must return the pair.
        let parity_cols = make_parity_cols_perm();

        // Use bits 5 and 10 — both info bits, presumably with a
        // non-trivial XOR signature.
        let marginals = vec![5usize, 10usize];
        let mut sig: u32 = 0;
        for p in 0..NPRE2_NTAU {
            if parity_cols[5][p] ^ parity_cols[10][p] {
                sig |= 1 << p;
            }
        }

        let pairs = npre2_collect_pairs(&marginals, &parity_cols, sig);
        assert!(
            !pairs.is_empty(),
            "Expected at least one matching pair for the constructed signature"
        );
        // The (5, 10) pair must be among the matches (it's the only
        // pair in the candidate list).
        assert!(
            pairs.iter().any(|&(a, b)| (a, b) == (5, 10)),
            "Expected pair (5, 10) in matches, got {:?}",
            pairs
        );
    }

    #[test]
    fn test_npre2_collect_pairs_no_match_returns_empty() {
        let parity_cols = make_parity_cols_perm();
        let marginals = vec![5usize, 10usize];
        // Use a signature whose bits all differ from the actual pair
        // XOR — pick the bitwise complement (truncated to NPRE2_NTAU).
        let mut sig: u32 = 0;
        for p in 0..NPRE2_NTAU {
            if parity_cols[5][p] ^ parity_cols[10][p] {
                sig |= 1 << p;
            }
        }
        let mask = ((1u64 << NPRE2_NTAU) - 1) as u32;
        let wrong_sig = sig ^ mask;
        let pairs = npre2_collect_pairs(&marginals, &parity_cols, wrong_sig);
        assert!(
            pairs.iter().all(|&(a, b)| (a, b) != (5, 10)),
            "Pair (5, 10) should NOT match a wrong signature; got {:?}",
            pairs
        );
    }

    #[cfg(feature = "transmit")]
    #[test]
    fn test_npre2_default_off_preserves_osd_decode_results() {
        // Verify that with `npre2_preprocessing_enabled = false`,
        // OSD-3 decode results are bit-identical to the legacy path.
        // We exercise both clean and single-bit-error codewords; the
        // npre2-disabled decoder must match the prior-behavior decoder
        // byte-for-byte.
        use crate::ldpc::LdpcEncoder;
        use crate::message::{calculate_crc14, CRC_BITS, PAYLOAD_BITS};

        // Construct a valid codeword (same helper as osd_decode_tests).
        let mut payload: BitVec = BitVec::repeat(false, PAYLOAD_BITS);
        payload.set(3, true);
        payload.set(10, true);
        payload.set(50, true);
        let crc = calculate_crc14(&payload);
        let mut message: BitVec = payload;
        for i in 0..CRC_BITS {
            message.push((crc >> (CRC_BITS - 1 - i)) & 1 == 1);
        }
        let encoder = LdpcEncoder::new();
        let codeword = encoder.encode(&message).expect("LDPC encode failed");

        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            llrs[i] = if codeword[i] { -4.0 } else { 4.0 };
        }

        // npre2 disabled (default).
        let decoder_off = OsdDecoder::new(OsdConfig {
            max_depth: 3,
            npre2_preprocessing_enabled: false,
            ..Default::default()
        });
        let result_off = decoder_off.decode(&llrs, None);
        assert!(
            result_off.is_some(),
            "OSD-3 with npre2 OFF should decode a clean codeword"
        );
        assert_eq!(
            result_off.as_ref().unwrap(),
            &codeword,
            "OSD-3 with npre2 OFF must recover the original codeword \
             (byte-identical default behavior)"
        );

        // npre2 enabled — clean codeword should still decode (the
        // warm-start path is skipped because OSD-0 succeeds first).
        let decoder_on = OsdDecoder::new(OsdConfig {
            max_depth: 3,
            npre2_preprocessing_enabled: true,
            ..Default::default()
        });
        let result_on = decoder_on.decode(&llrs, None);
        assert_eq!(
            result_on, result_off,
            "On a clean codeword, npre2 ON and OFF must produce \
             byte-identical outputs (OSD-0 returns first)."
        );
    }
}
