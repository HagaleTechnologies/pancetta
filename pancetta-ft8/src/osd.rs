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

use crate::ldpc::{
    LDPC_CODEWORD_BITS, LDPC_GENERATOR, LDPC_INFO_BITS, LDPC_PARITY_BITS,
};
use crate::message::{calculate_crc14, CRC_BITS, PAYLOAD_BITS};

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
        Self { max_depth: 2 }
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

/// Clear bit at position `col` to 0 in a packed row (MSB-first).
#[inline]
fn clear_bit(row: &mut PackedRow, col: usize) {
    row[col / 8] &= !(1 << (7 - (col % 8)));
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
                if let Some(_) = donor_row {
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
            if found.is_none() {
                return None;
            }
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

        // Test clear
        clear_bit(&mut row, 0);
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
        assert!(get_bit(&a, 5));  // 1 ^ 0 = 1
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
}
