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

use bitvec::prelude::*;

use crate::ldpc::{LDPC_CODEWORD_BITS, LDPC_GENERATOR, LDPC_INFO_BITS, LDPC_PARITY_BITS};
use crate::message::{CRC_BITS, PAYLOAD_BITS};

/// Number of bytes needed to pack a full 174-bit codeword row (ceil(174/8)).
const PACKED_BYTES: usize = 22;

/// Configuration for OSD decoding.
#[derive(Debug, Clone, Copy)]
pub struct OsdConfig {
    /// Maximum OSD order (number of bit flips to try).
    /// Order 0 = hard decision only, order 1 = single bit flips,
    /// order 2 = all pairs of bit flips, etc.
    pub max_depth: u8,
}

impl Default for OsdConfig {
    fn default() -> Self {
        // OSD-1 is the safe default. OSD-2 (4,187 trials) has a high
        // CRC-14 false positive rate without additional validation.
        Self { max_depth: 1 }
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

/// OSD decoder that attempts to decode LLRs using ordered statistics decoding
/// at depths 0, 1, and 2 with CRC-14 validation.
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
    #[allow(clippy::needless_range_loop)]
    pub fn decode(&self, llrs: &[f32; LDPC_CODEWORD_BITS]) -> Option<BitVec> {
        // 1. Sort indices by descending |LLR| (most reliable first)
        let mut sorted_indices: [usize; LDPC_CODEWORD_BITS] = [0; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            sorted_indices[i] = i;
        }
        sorted_indices.sort_by(|&a, &b| {
            llrs[b]
                .abs()
                .partial_cmp(&llrs[a].abs())
                .unwrap_or(std::cmp::Ordering::Equal)
        });

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
            return Some(result);
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
                return Some(result);
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
                    return Some(result);
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

            let decoder = OsdDecoder::new(OsdConfig { max_depth: 0 });
            let result = decoder.decode(&llrs);

            assert!(result.is_some(), "OSD-0 should decode a clean codeword");
            let decoded = result.unwrap();
            assert_eq!(decoded.len(), LDPC_CODEWORD_BITS);
            assert_eq!(decoded, codeword, "Decoded codeword should match original");
        }

        #[test]
        fn test_osd1_recovers_single_unreliable_bit() {
            let (_message, codeword) = make_test_codeword();
            let mut llrs = codeword_to_llrs(&codeword, 4.0);

            // Make one bit wrong-signed and low magnitude (unreliable and incorrect)
            llrs[5] = if codeword[5] { 0.1 } else { -0.1 };

            // OSD-0 should fail
            let decoder0 = OsdDecoder::new(OsdConfig { max_depth: 0 });
            assert!(
                decoder0.decode(&llrs).is_none(),
                "OSD-0 should fail with one corrupted bit"
            );

            // OSD-1 should succeed
            let decoder1 = OsdDecoder::new(OsdConfig { max_depth: 1 });
            let result = decoder1.decode(&llrs);
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

            let decoder1 = OsdDecoder::new(OsdConfig { max_depth: 1 });
            let decoder2 = OsdDecoder::new(OsdConfig { max_depth: 2 });

            let mut found_good_pair = false;
            for &(a, b) in &pairs {
                let mut llrs = codeword_to_llrs(&codeword, 4.0);
                // Wrong-sign with small magnitude
                llrs[a] = if codeword[a] { 0.05 } else { -0.05 };
                llrs[b] = if codeword[b] { 0.05 } else { -0.05 };

                let osd1_result = decoder1.decode(&llrs);
                let osd2_result = decoder2.decode(&llrs);

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
    }
}
