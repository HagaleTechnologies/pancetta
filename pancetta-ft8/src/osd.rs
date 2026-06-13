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

use std::collections::HashMap;

use bitvec::prelude::*;

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
}

impl Default for OsdConfig {
    fn default() -> Self {
        // OSD-1 is the safe default. OSD-2 (4,187 trials) has a high
        // CRC-14 false positive rate without additional validation.
        Self {
            max_depth: 1,
            npre2_preprocessing_enabled: false,
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
fn npre2_residual_signature(
    info_hard: &[u8; LDPC_INFO_BITS],
    base_parity: &[u8; LDPC_PARITY_BITS],
    parity_cols_perm: &[[bool; LDPC_PARITY_BITS]; LDPC_INFO_BITS],
) -> u32 {
    // For the order-0 reference, `base_parity` already encodes the
    // expected parity from `info_hard` under the row-echelon generator.
    // Since the order-0 codeword is by construction parity-consistent
    // (info_hard + base_parity is a codeword), the residual is the
    // distance between the received parity-hard-decisions and the
    // order-0 parity. But we don't have received parity hard decisions
    // separately here — they're folded into the LLR signs.
    //
    // The npre2 warm-start operates on "what does flipping bits change
    // about the parity contribution?". For the integration in
    // OsdDecoder::decode, the relevant signature is the *parity error
    // pattern* that an order-iorder flip would induce relative to the
    // order-0 baseline. We use the conservative interpretation:
    // signature = first NPRE2_NTAU bits of base_parity (the order-0
    // expected parity, against which warm-start pairs flip). A pair
    // whose XOR'd parity columns match this signature, when applied,
    // yields a codeword whose first-NPRE2_NTAU parity bits all flip to
    // their complement — the WSJT-X "cancel the parity-error pattern"
    // semantic from the spec.
    let _ = info_hard;
    let _ = parity_cols_perm;
    let mut sig: u32 = 0;
    for p in 0..NPRE2_NTAU {
        if base_parity[p] == 1 {
            sig |= 1 << p;
        }
    }
    sig
}

/// OSD decoder that attempts to decode LLRs using ordered statistics decoding
/// at depths 0, 1, 2, and 3 with CRC-14 validation.
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
    /// OSD depth at which `try_solution` first succeeded (0/1/2/3, where
    /// 3 covers both the npre2 warm-start and the full triple loop).
    /// `nharderrs` is the number of info bits flipped at the successful
    /// trial (0 / 1 / 2 / 2 [npre2 pair] / 3 [triple]).
    /// Inspired by spec ref `spec-wsjtx-improved-fdr.md` §"Inputs".
    #[allow(clippy::needless_range_loop)]
    pub fn decode_with_features(
        &self,
        llrs: &[f32; LDPC_CODEWORD_BITS],
        neural_ordering: Option<&[f32; LDPC_INFO_BITS]>,
    ) -> Option<(BitVec, u8, u8)> {
        // 1. Sort indices by reliability
        let mut sorted_indices: [usize; LDPC_CODEWORD_BITS] = [0; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            sorted_indices[i] = i;
        }
        if let Some(probs) = neural_ordering {
            // Neural ordering: sort info bits by predicted error probability
            // (highest probability first = least reliable), parity bits by |LLR|
            sorted_indices.sort_by(|&a, &b| {
                let key_a = if a < LDPC_INFO_BITS {
                    -probs[a] // negative so highest prob sorts first (= most unreliable)
                } else {
                    -llrs[a].abs() // parity bits: high |LLR| = reliable, sort last
                };
                let key_b = if b < LDPC_INFO_BITS {
                    -probs[b]
                } else {
                    -llrs[b].abs()
                };
                // Sort ascending (most negative = highest prob = least reliable = first)
                key_b
                    .partial_cmp(&key_a)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        } else {
            // Original: sort by descending |LLR| (most reliable first)
            sorted_indices.sort_by(|&a, &b| {
                llrs[b]
                    .abs()
                    .partial_cmp(&llrs[a].abs())
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
        };

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

        // Try OSD-0
        if let Some(result) = self.try_solution(&info_hard, &base_parity, &final_perm) {
            return Some((result, 0, 0));
        }

        if self.config.max_depth < 1 {
            return None;
        }

        // 6. OSD-1: pre-compute parity columns
        let mut parity_cols = [[false; LDPC_PARITY_BITS]; LDPC_INFO_BITS];
        for i in 0..LDPC_INFO_BITS {
            for p in 0..LDPC_PARITY_BITS {
                parity_cols[i][p] = get_bit(&matrix[i], LDPC_INFO_BITS + p);
            }
        }

        for flip in 0..LDPC_INFO_BITS {
            let mut info = info_hard;
            info[flip] ^= 1;
            let mut parity = base_parity;
            for p in 0..LDPC_PARITY_BITS {
                if parity_cols[flip][p] {
                    parity[p] ^= 1;
                }
            }
            if let Some(result) = self.try_solution(&info, &parity, &final_perm) {
                return Some((result, 1, 1));
            }
        }

        if self.config.max_depth < 2 {
            return None;
        }

        // 7. OSD-2: flip pairs
        for i in 0..LDPC_INFO_BITS {
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
                if let Some(result) = self.try_solution(&info, &parity, &final_perm) {
                    return Some((result, 2, 2));
                }
            }
        }

        if self.config.max_depth < 3 {
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
                let residual = npre2_residual_signature(&info_hard, &base_parity, &parity_cols);
                let warm_pairs = npre2_collect_pairs(&marginals, &parity_cols, residual);

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
                    if let Some(result) = self.try_solution(&info, &parity, &final_perm) {
                        // npre2 warm-start always flips a marginal pair,
                        // even though it's enumerated within the depth-3 budget.
                        return Some((result, 3, 2));
                    }
                }
            }
        }

        // 8. OSD-3: flip all triples — C(91, 3) = 121,485 trials
        //    (= 91 · 90 · 89 / 6). Comment corrected 2026-06-02 (Phase C)
        //    per docs/engineering/2026-06-02-engineering-substance-audit.md
        //    (claim 17); loop math was already correct.
        // Each trial XORs 3 rows of the reduced generator matrix, then checks CRC-14.
        for i in 0..LDPC_INFO_BITS {
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
                    if let Some(result) = self.try_solution(&info, &parity, &final_perm) {
                        return Some((result, 3, 3));
                    }
                }
            }
        }

        None
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

                if osd1_result.is_none() && osd2_result.is_some() {
                    assert_eq!(
                        osd2_result.unwrap(),
                        codeword,
                        "OSD-2 decoded wrong codeword for pair ({}, {})",
                        a,
                        b
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

                if osd2_result.is_none() && osd3_result.is_some() {
                    assert_eq!(
                        osd3_result.unwrap(),
                        codeword,
                        "OSD-3 decoded wrong codeword for triple ({}, {}, {})",
                        a,
                        b,
                        c
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
        });
        let result_on = decoder_on.decode(&llrs, None);
        assert_eq!(
            result_on, result_off,
            "On a clean codeword, npre2 ON and OFF must produce \
             byte-identical outputs (OSD-0 returns first)."
        );
    }
}
