# OSD Decoder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add OSD-0/1/2 as an LDPC BP fallback to recover weak FT8 signals, improving decode sensitivity by ~2 dB.

**Architecture:** New `osd.rs` module with `OsdDecoder` struct. Integrates into the existing `LdpcDecoder` in `decoder.rs` as an optional fallback when BP fails to converge. Uses the LDPC generator matrix from `ldpc.rs` to construct a systematic 91×174 generator for GF(2) row operations.

**Tech Stack:** Rust, bitvec, existing LDPC/CRC infrastructure in pancetta-ft8

**Spec:** `docs/superpowers/specs/2026-03-30-osd-decoder-design.md`

---

## File Structure

| File | Action | Responsibility |
|---|---|---|
| `pancetta-ft8/src/osd.rs` | Create | OSD decoder: config, GF(2) matrix ops, OSD-0/1/2 search |
| `pancetta-ft8/src/lib.rs` | Modify (1 line) | Add `pub mod osd;` |
| `pancetta-ft8/src/decoder.rs` | Modify | Add `osd_depth` to `Ft8Config`, add OSD field to `LdpcDecoder`, call OSD on BP failure |
| `pancetta-ft8/tests/osd_tests.rs` | Create | Integration tests: weak-signal OSD recovery, no false positives |
| `benchmarks/BASELINE.md` | Modify | Record post-OSD benchmark results |

---

### Task 1: Create `osd.rs` with GF(2) matrix types and Gaussian elimination

**Files:**
- Create: `pancetta-ft8/src/osd.rs`
- Modify: `pancetta-ft8/src/lib.rs:59` (add module declaration)

- [ ] **Step 1: Write the test for Gaussian elimination**

In `pancetta-ft8/src/osd.rs`, add the module with config, matrix type, and a test:

```rust
//! Ordered Statistics Decoding (OSD) for the FT8 LDPC(174,91) code.
//!
//! OSD is a fallback decoder that runs when belief propagation fails to converge.
//! It sorts bits by reliability, Gaussian-eliminates the generator matrix, and
//! searches for valid codewords by flipping least-reliable bits.

use bitvec::prelude::*;

use crate::ldpc::{LDPC_INFO_BITS, LDPC_PARITY_BITS, LDPC_CODEWORD_BITS};
use crate::message::{calculate_crc14, PAYLOAD_BITS, CRC_BITS};

/// Number of packed bytes per codeword row: ceil(174/8) = 22
const PACKED_BYTES: usize = 22;

/// Configuration for OSD depth.
#[derive(Debug, Clone, Copy)]
pub struct OsdConfig {
    /// Maximum OSD order: 0, 1, or 2. Default: 2.
    pub max_depth: u8,
}

impl Default for OsdConfig {
    fn default() -> Self {
        Self { max_depth: 2 }
    }
}

/// A row of the generator matrix packed into bytes (174 bits in 22 bytes, MSB first).
type PackedRow = [u8; PACKED_BYTES];

/// Get bit `col` from a packed row (MSB-first packing).
#[inline]
fn get_bit(row: &PackedRow, col: usize) -> bool {
    (row[col / 8] >> (7 - (col % 8))) & 1 != 0
}

/// Set bit `col` in a packed row to 1.
#[inline]
fn set_bit(row: &mut PackedRow, col: usize) {
    row[col / 8] |= 1 << (7 - (col % 8));
}

/// Clear bit `col` in a packed row to 0.
#[inline]
fn clear_bit(row: &mut PackedRow, col: usize) {
    row[col / 8] &= !(1 << (7 - (col % 8)));
}

/// Flip bit `col` in a packed row.
#[inline]
fn flip_bit(row: &mut PackedRow, col: usize) {
    row[col / 8] ^= 1 << (7 - (col % 8));
}

/// XOR row `src` into row `dst` (dst ^= src).
#[inline]
fn xor_rows(dst: &mut PackedRow, src: &PackedRow) {
    for i in 0..PACKED_BYTES {
        dst[i] ^= src[i];
    }
}

/// Build the systematic generator matrix G = [I_91 | P] from the LDPC generator.
///
/// The LDPC generator from ldpc.rs is P: 83 rows × 12 bytes (91 info bits → 83 parity bits).
/// We construct the full 91×174 matrix where:
/// - Columns 0..90 = I_91 (identity)
/// - Columns 91..173 = P^T (transposed parity)
fn build_systematic_generator() -> [[u8; PACKED_BYTES]; LDPC_INFO_BITS] {
    use crate::ldpc::LDPC_GENERATOR;

    let mut g = [[0u8; PACKED_BYTES]; LDPC_INFO_BITS];

    for info_bit in 0..LDPC_INFO_BITS {
        // Identity part: set column `info_bit` to 1
        set_bit(&mut g[info_bit], info_bit);

        // Parity part: for each parity row p, if generator[p] has info_bit set,
        // then this info bit contributes to parity bit p.
        // Parity bit p is at column (91 + p) in the full codeword.
        for p in 0..LDPC_PARITY_BITS {
            let gen_byte = info_bit / 8;
            let gen_bit = 7 - (info_bit % 8);
            if (LDPC_GENERATOR[p][gen_byte] >> gen_bit) & 1 != 0 {
                set_bit(&mut g[info_bit], LDPC_INFO_BITS + p);
            }
        }
    }

    g
}

/// Gaussian elimination on a 91×174 binary matrix to produce systematic form.
///
/// After elimination, the first 91 columns (in permuted order) form an identity.
/// `col_perm` tracks column permutations: col_perm[i] = original column index.
///
/// Returns `None` if the matrix is singular (rank < 91).
fn gaussian_eliminate(
    matrix: &mut [[u8; PACKED_BYTES]; LDPC_INFO_BITS],
    col_perm: &mut [usize; LDPC_CODEWORD_BITS],
) -> Option<()> {
    for row in 0..LDPC_INFO_BITS {
        // Find pivot: look for a 1 in column `row` (after permutation) in rows `row..91`
        let mut pivot_found = false;

        // First try to find a pivot in the current column among remaining rows
        for search_row in row..LDPC_INFO_BITS {
            if get_bit(&matrix[search_row], row) {
                // Swap rows
                if search_row != row {
                    matrix.swap(search_row, row);
                }
                pivot_found = true;
                break;
            }
        }

        // If no pivot in current column, swap with a column from the right side
        if !pivot_found {
            let mut swap_col = None;
            for col in (LDPC_INFO_BITS)..LDPC_CODEWORD_BITS {
                if get_bit(&matrix[row], col) {
                    swap_col = Some(col);
                    break;
                }
            }

            let col = swap_col?; // Return None if truly singular

            // Swap columns `row` and `col` in all rows
            for r in 0..LDPC_INFO_BITS {
                let bit_a = get_bit(&matrix[r], row);
                let bit_b = get_bit(&matrix[r], col);
                if bit_a {
                    set_bit(&mut matrix[r], col);
                } else {
                    clear_bit(&mut matrix[r], col);
                }
                if bit_b {
                    set_bit(&mut matrix[r], row);
                } else {
                    clear_bit(&mut matrix[r], row);
                }
            }
            col_perm.swap(row, col);

            // Now find pivot in the swapped column
            for search_row in row..LDPC_INFO_BITS {
                if get_bit(&matrix[search_row], row) {
                    if search_row != row {
                        matrix.swap(search_row, row);
                    }
                    pivot_found = true;
                    break;
                }
            }

            if !pivot_found {
                return None;
            }
        }

        // Eliminate: XOR this row into all other rows that have a 1 in column `row`
        // Clone the pivot row to avoid borrow conflicts
        let pivot_row = matrix[row];
        for other_row in 0..LDPC_INFO_BITS {
            if other_row != row && get_bit(&matrix[other_row], row) {
                xor_rows(&mut matrix[other_row], &pivot_row);
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
        let mut row = [0u8; PACKED_BYTES];
        assert!(!get_bit(&row, 0));

        set_bit(&mut row, 0);
        assert!(get_bit(&row, 0));

        set_bit(&mut row, 173);
        assert!(get_bit(&row, 173));

        clear_bit(&mut row, 0);
        assert!(!get_bit(&row, 0));

        flip_bit(&mut row, 100);
        assert!(get_bit(&row, 100));
        flip_bit(&mut row, 100);
        assert!(!get_bit(&row, 100));
    }

    #[test]
    fn test_build_systematic_generator() {
        let g = build_systematic_generator();

        // Identity part: row i should have bit i set in columns 0..91
        for i in 0..LDPC_INFO_BITS {
            for j in 0..LDPC_INFO_BITS {
                if i == j {
                    assert!(get_bit(&g[i], j), "Identity diagonal missing at ({}, {})", i, j);
                } else {
                    assert!(!get_bit(&g[i], j), "Non-zero off-diagonal at ({}, {})", i, j);
                }
            }
        }

        // Parity part should have some non-zero entries
        let mut parity_ones = 0;
        for i in 0..LDPC_INFO_BITS {
            for j in LDPC_INFO_BITS..LDPC_CODEWORD_BITS {
                if get_bit(&g[i], j) {
                    parity_ones += 1;
                }
            }
        }
        assert!(parity_ones > 0, "Parity section should have non-zero entries");
    }

    #[test]
    fn test_gaussian_elimination_produces_identity() {
        let mut g = build_systematic_generator();
        let mut col_perm: [usize; LDPC_CODEWORD_BITS] = std::array::from_fn(|i| i);

        let result = gaussian_eliminate(&mut g, &mut col_perm);
        assert!(result.is_some(), "Gaussian elimination should succeed on LDPC generator");

        // After elimination, columns col_perm[0..91] should form identity
        for i in 0..LDPC_INFO_BITS {
            assert!(get_bit(&g[i], i), "Diagonal not set at row {}", i);
            for j in 0..LDPC_INFO_BITS {
                if i != j {
                    assert!(!get_bit(&g[i], j), "Off-diagonal set at ({}, {})", i, j);
                }
            }
        }
    }
}
```

- [ ] **Step 2: Add module declaration to lib.rs**

In `pancetta-ft8/src/lib.rs`, after line 60 (`pub mod message;`), add:

```rust
pub mod osd;
```

- [ ] **Step 3: Make `LDPC_GENERATOR` accessible from osd.rs**

In `pancetta-ft8/src/ldpc.rs`, change the generator matrix visibility from `const` to `pub(crate) const`:

```rust
// Change:
const LDPC_GENERATOR: [[u8; 12]; 83] = [
// To:
pub(crate) const LDPC_GENERATOR: [[u8; 12]; 83] = [
```

- [ ] **Step 4: Run tests to verify**

Run: `cargo test -p pancetta-ft8 osd --lib`
Expected: 3 tests pass (bit_operations, build_systematic_generator, gaussian_elimination_produces_identity)

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/src/osd.rs pancetta-ft8/src/lib.rs pancetta-ft8/src/ldpc.rs
git commit -m "feat(osd): add GF(2) matrix types and Gaussian elimination"
```

---

### Task 2: Implement OSD-0/1/2 decode with CRC-14 validation

**Files:**
- Modify: `pancetta-ft8/src/osd.rs`

- [ ] **Step 1: Write tests for OSD-0 on a clean codeword**

Add to `osd.rs` tests module:

```rust
    #[test]
    fn test_osd0_recovers_clean_codeword() {
        // Encode a known message to get a valid 174-bit codeword
        use crate::ldpc::LdpcEncoder;

        let encoder = LdpcEncoder::new();
        // "CQ W1ABC FN42" — arbitrary valid payload
        // We'll use all-zeros payload for simplicity (valid CRC = known value)
        let mut message = bitvec![0; LDPC_INFO_BITS];
        // Set some bits to make it non-trivial
        message.set(0, true);
        message.set(10, true);
        message.set(20, true);

        let codeword = encoder.encode(&message).unwrap();

        // Convert codeword bits to LLRs: 0 → +4.0, 1 → -4.0
        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            llrs[i] = if codeword[i] { -4.0 } else { 4.0 };
        }

        let osd = OsdDecoder::new(OsdConfig { max_depth: 0 });
        let result = osd.decode(&llrs);
        assert!(result.is_some(), "OSD-0 should recover a clean codeword");

        let decoded = result.unwrap();
        assert_eq!(decoded.len(), LDPC_CODEWORD_BITS);
        for i in 0..LDPC_CODEWORD_BITS {
            assert_eq!(decoded[i], codeword[i], "Bit {} mismatch", i);
        }
    }
```

- [ ] **Step 2: Write test for OSD-1 recovering a 1-bit-unreliable codeword**

```rust
    #[test]
    fn test_osd1_recovers_single_unreliable_bit() {
        use crate::ldpc::LdpcEncoder;

        let encoder = LdpcEncoder::new();
        let mut message = bitvec![0; LDPC_INFO_BITS];
        message.set(5, true);
        message.set(15, true);
        message.set(50, true);

        let codeword = encoder.encode(&message).unwrap();

        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            llrs[i] = if codeword[i] { -4.0 } else { 4.0 };
        }

        // Flip bit 5's LLR sign (make it unreliable AND wrong)
        llrs[5] = 0.1; // Very small magnitude, wrong sign (bit 5 is 1, should be negative)

        // OSD-0 alone won't recover (bit 5 hard-decides wrong)
        let osd0 = OsdDecoder::new(OsdConfig { max_depth: 0 });
        assert!(osd0.decode(&llrs).is_none(), "OSD-0 should fail with wrong bit");

        // OSD-1 should recover by flipping the least reliable bit
        let osd1 = OsdDecoder::new(OsdConfig { max_depth: 1 });
        let result = osd1.decode(&llrs);
        assert!(result.is_some(), "OSD-1 should recover a single unreliable bit");

        let decoded = result.unwrap();
        for i in 0..LDPC_CODEWORD_BITS {
            assert_eq!(decoded[i], codeword[i], "Bit {} mismatch", i);
        }
    }
```

- [ ] **Step 3: Write test for OSD-2 recovering 2 unreliable bits**

```rust
    #[test]
    fn test_osd2_recovers_two_unreliable_bits() {
        use crate::ldpc::LdpcEncoder;

        let encoder = LdpcEncoder::new();
        let mut message = bitvec![0; LDPC_INFO_BITS];
        message.set(3, true);
        message.set(30, true);
        message.set(60, true);

        let codeword = encoder.encode(&message).unwrap();

        let mut llrs = [0.0f32; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            llrs[i] = if codeword[i] { -4.0 } else { 4.0 };
        }

        // Flip two bits' LLR signs
        llrs[3] = 0.1;  // bit 3 is 1, should be negative
        llrs[30] = -0.1; // bit 30 is 1, should be negative, but make it barely right —
                          // actually let's make it wrong too
        llrs[30] = 0.1;

        // OSD-1 should fail (two bits wrong)
        let osd1 = OsdDecoder::new(OsdConfig { max_depth: 1 });
        assert!(osd1.decode(&llrs).is_none(), "OSD-1 should fail with two wrong bits");

        // OSD-2 should recover
        let osd2 = OsdDecoder::new(OsdConfig { max_depth: 2 });
        let result = osd2.decode(&llrs);
        assert!(result.is_some(), "OSD-2 should recover two unreliable bits");

        let decoded = result.unwrap();
        for i in 0..LDPC_CODEWORD_BITS {
            assert_eq!(decoded[i], codeword[i], "Bit {} mismatch", i);
        }
    }
```

- [ ] **Step 4: Implement `OsdDecoder`**

Add to `osd.rs` before the tests module:

```rust
/// Ordered Statistics Decoder for the FT8 LDPC(174,91) code.
pub struct OsdDecoder {
    config: OsdConfig,
    /// Systematic generator matrix: 91 rows × 22 bytes (174 bits).
    /// G = [I_91 | P], where P is derived from LDPC_GENERATOR.
    generator: [[u8; PACKED_BYTES]; LDPC_INFO_BITS],
}

impl OsdDecoder {
    /// Create a new OSD decoder.
    pub fn new(config: OsdConfig) -> Self {
        let generator = build_systematic_generator();
        Self { config, generator }
    }

    /// Attempt OSD decode given the channel LLRs from BP.
    ///
    /// Returns the decoded 174-bit codeword if a valid codeword (CRC-14 pass)
    /// is found at any depth up to `max_depth`. Returns `None` otherwise.
    pub fn decode(&self, llrs: &[f32; LDPC_CODEWORD_BITS]) -> Option<BitVec> {
        // Step 1: Sort bits by reliability (descending |LLR|)
        let mut indices: [usize; LDPC_CODEWORD_BITS] = std::array::from_fn(|i| i);
        indices.sort_by(|&a, &b| {
            llrs[b].abs().partial_cmp(&llrs[a].abs()).unwrap_or(std::cmp::Ordering::Equal)
        });

        // col_perm[i] = original column index for position i in the permuted matrix
        let mut col_perm: [usize; LDPC_CODEWORD_BITS] = indices;

        // Step 2: Permute generator matrix columns according to reliability order
        let mut matrix = self.generator;
        let mut permuted = [[0u8; PACKED_BYTES]; LDPC_INFO_BITS];
        for row in 0..LDPC_INFO_BITS {
            for new_col in 0..LDPC_CODEWORD_BITS {
                let orig_col = col_perm[new_col];
                if get_bit(&matrix[row], orig_col) {
                    set_bit(&mut permuted[row], new_col);
                }
            }
        }
        matrix = permuted;

        // Step 3: Gaussian elimination
        // We need col_perm to track further swaps during elimination
        let mut elim_perm: [usize; LDPC_CODEWORD_BITS] = std::array::from_fn(|i| i);
        gaussian_eliminate(&mut matrix, &mut elim_perm)?;

        // Compose permutations: final_perm[i] = col_perm[elim_perm[i]]
        let mut final_perm: [usize; LDPC_CODEWORD_BITS] = [0; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_CODEWORD_BITS {
            final_perm[i] = col_perm[elim_perm[i]];
        }

        // Step 4: Hard-decide the 91 most-reliable bits (in permuted order)
        // The first 91 positions (after elimination) correspond to info bits
        let mut info_hard = [0u8; LDPC_INFO_BITS];
        for i in 0..LDPC_INFO_BITS {
            let orig_col = final_perm[i];
            info_hard[i] = if llrs[orig_col] < 0.0 { 1 } else { 0 };
        }

        // Compute parity from the reduced generator matrix
        // After Gaussian elimination, matrix has identity in first 91 cols,
        // so parity bit p = sum of (info_hard[i] * matrix[i][91+p]) for all i
        let mut base_parity = [0u8; LDPC_PARITY_BITS];
        for p in 0..LDPC_PARITY_BITS {
            let mut sum = 0u8;
            for i in 0..LDPC_INFO_BITS {
                if info_hard[i] == 1 && get_bit(&matrix[i], LDPC_INFO_BITS + p) {
                    sum ^= 1;
                }
            }
            base_parity[p] = sum;
        }

        // Build the base codeword (in permuted order) and check CRC
        // OSD-0: just try the hard-decided solution
        if let Some(bits) = self.try_solution(&info_hard, &base_parity, &final_perm) {
            return Some(bits);
        }

        if self.config.max_depth == 0 {
            return None;
        }

        // Pre-compute parity columns for efficient flipping:
        // parity_col[i][p] = matrix[i][91+p] — whether flipping info bit i toggles parity bit p
        let mut parity_cols = [[false; LDPC_PARITY_BITS]; LDPC_INFO_BITS];
        for i in 0..LDPC_INFO_BITS {
            for p in 0..LDPC_PARITY_BITS {
                parity_cols[i][p] = get_bit(&matrix[i], LDPC_INFO_BITS + p);
            }
        }

        // OSD-1: flip each info bit one at a time
        for flip in 0..LDPC_INFO_BITS {
            let mut trial_info = info_hard;
            trial_info[flip] ^= 1;

            let mut trial_parity = base_parity;
            for p in 0..LDPC_PARITY_BITS {
                if parity_cols[flip][p] {
                    trial_parity[p] ^= 1;
                }
            }

            if let Some(bits) = self.try_solution(&trial_info, &trial_parity, &final_perm) {
                return Some(bits);
            }
        }

        if self.config.max_depth < 2 {
            return None;
        }

        // OSD-2: flip pairs of info bits
        for i in 0..LDPC_INFO_BITS {
            for j in (i + 1)..LDPC_INFO_BITS {
                let mut trial_info = info_hard;
                trial_info[i] ^= 1;
                trial_info[j] ^= 1;

                let mut trial_parity = base_parity;
                for p in 0..LDPC_PARITY_BITS {
                    if parity_cols[i][p] {
                        trial_parity[p] ^= 1;
                    }
                    if parity_cols[j][p] {
                        trial_parity[p] ^= 1;
                    }
                }

                if let Some(bits) = self.try_solution(&trial_info, &trial_parity, &final_perm) {
                    return Some(bits);
                }
            }
        }

        None
    }

    /// Build a full codeword from info + parity bits, un-permute, and check CRC-14.
    /// Returns the un-permuted codeword as a BitVec if CRC passes.
    fn try_solution(
        &self,
        info: &[u8; LDPC_INFO_BITS],
        parity: &[u8; LDPC_PARITY_BITS],
        final_perm: &[usize; LDPC_CODEWORD_BITS],
    ) -> Option<BitVec> {
        // Un-permute: place each bit back in its original column position
        let mut codeword = [0u8; LDPC_CODEWORD_BITS];
        for i in 0..LDPC_INFO_BITS {
            codeword[final_perm[i]] = info[i];
        }
        for p in 0..LDPC_PARITY_BITS {
            codeword[final_perm[LDPC_INFO_BITS + p]] = parity[p];
        }

        // Check CRC-14 on the first 91 bits (77 payload + 14 CRC)
        let info_bits: BitVec = codeword[..LDPC_INFO_BITS]
            .iter()
            .map(|&b| b == 1)
            .collect();

        let payload = &info_bits[0..PAYLOAD_BITS];
        let crc_bits = &info_bits[PAYLOAD_BITS..PAYLOAD_BITS + CRC_BITS];

        let calculated_crc = calculate_crc14(payload);
        let mut received_crc = 0u16;
        for (i, bit) in crc_bits.iter().enumerate() {
            if *bit {
                received_crc |= 1 << (CRC_BITS - 1 - i);
            }
        }

        if calculated_crc != received_crc {
            return None;
        }

        // CRC passed — return the full 174-bit codeword
        Some(codeword.iter().map(|&b| b == 1).collect())
    }
}
```

- [ ] **Step 5: Run tests to verify**

Run: `cargo test -p pancetta-ft8 osd --lib`
Expected: 6 tests pass (3 from Task 1 + 3 new OSD decode tests)

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/osd.rs
git commit -m "feat(osd): implement OSD-0/1/2 decode with CRC-14 validation"
```

---

### Task 3: Integrate OSD into LdpcDecoder as BP fallback

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:71-116` (Ft8Config — add `osd_depth`)
- Modify: `pancetta-ft8/src/decoder.rs:1211-1250` (LdpcDecoder — add OSD field)
- Modify: `pancetta-ft8/src/decoder.rs:1260-1270` (decode_soft — add OSD fallback)

- [ ] **Step 1: Write a test for BP+OSD fallback**

Add to `pancetta-ft8/src/decoder.rs` tests module:

```rust
    #[test]
    fn test_ldpc_decode_soft_with_osd_fallback() {
        use crate::osd::OsdConfig;

        // Create decoder with OSD enabled
        let decoder = LdpcDecoder::new_with_osd(50, Some(OsdConfig { max_depth: 2 })).unwrap();

        // Encode a known message
        use crate::ldpc::LdpcEncoder;
        let encoder = LdpcEncoder::new();
        let mut message = bitvec![0; 91];
        message.set(0, true);
        message.set(10, true);

        let codeword = encoder.encode(&message).unwrap();

        // Create LLRs with 2 unreliable bits (BP will fail, OSD-2 should recover)
        let mut llrs = [0.0f32; 174];
        for i in 0..174 {
            llrs[i] = if codeword[i] { -4.0 } else { 4.0 };
        }
        // Make 2 bits unreliable and wrong-signed
        llrs[0] = 0.05;
        llrs[10] = 0.05;

        // With only 1 BP iteration, BP won't converge
        let decoder_no_osd = LdpcDecoder::new(1).unwrap();
        let bp_result = decoder_no_osd.decode_soft(&llrs).unwrap();
        // BP with 1 iteration won't fix the errors — bits 0 and 10 will be wrong

        // With OSD, should recover
        let decoder_with_osd = LdpcDecoder::new_with_osd(1, Some(OsdConfig { max_depth: 2 })).unwrap();
        let osd_result = decoder_with_osd.decode_soft(&llrs).unwrap();
        for i in 0..174 {
            assert_eq!(osd_result[i], codeword[i], "Bit {} mismatch with OSD", i);
        }
    }
```

- [ ] **Step 2: Add `osd_depth` to `Ft8Config`**

In `pancetta-ft8/src/decoder.rs`, add to `Ft8Config` struct (after `max_decode_passes` field at line ~100):

```rust
    /// OSD depth (0, 1, or 2). Set to None to disable OSD. Default: Some(2).
    pub osd_depth: Option<u8>,
```

And in `Default` impl (after `max_decode_passes: 3,`):

```rust
            osd_depth: Some(2),
```

- [ ] **Step 3: Add OSD field to `LdpcDecoder` and `new_with_osd` constructor**

Add import at top of `decoder.rs`:

```rust
use crate::osd::{OsdConfig, OsdDecoder};
```

Modify `LdpcDecoder` struct to add the OSD field:

```rust
struct LdpcDecoder {
    max_iterations: usize,
    parity_check_matrix: ParityCheckMatrix,
    var_positions: Vec<Vec<(usize, usize)>>,
    normalization_factor: f32,
    /// Optional OSD fallback decoder
    osd: Option<OsdDecoder>,
}
```

Add `new_with_osd` constructor (keep existing `new` for backward compat):

```rust
    fn new_with_osd(max_iterations: usize, osd_config: Option<OsdConfig>) -> Ft8Result<Self> {
        let mut decoder = Self::new(max_iterations)?;
        decoder.osd = osd_config.map(OsdDecoder::new);
        Ok(decoder)
    }
```

Set `osd: None` in the existing `new` constructor's `Ok(Self { ... })` block.

- [ ] **Step 4: Modify `decode_soft` to try OSD on BP failure**

Replace the current `decode_soft` method:

```rust
    pub fn decode_soft(&self, llrs: &[f32]) -> Ft8Result<BitVec> {
        if llrs.len() != 174 {
            return Err(Ft8Error::InvalidDataSize {
                expected: 174,
                actual: llrs.len(),
            });
        }

        let decoded_llrs = self.belief_propagation(llrs)?;

        // Check if BP converged (syndrome = 0)
        let bp_converged = {
            let arr: &[f32; 174] = decoded_llrs[..174].try_into().unwrap();
            self.check_syndrome_fast(arr)
        };

        if bp_converged {
            return self.llrs_to_bits(&decoded_llrs);
        }

        // BP did not converge — try OSD fallback if available
        if let Some(ref osd) = self.osd {
            let llr_arr: &[f32; 174] = decoded_llrs[..174].try_into().unwrap();
            if let Some(codeword) = osd.decode(llr_arr) {
                return Ok(codeword);
            }
        }

        // Return BP's best effort (caller will check CRC and likely reject)
        self.llrs_to_bits(&decoded_llrs)
    }
```

- [ ] **Step 5: Wire OSD config into Ft8Decoder construction**

In `Ft8Decoder::with_message_handler` (line ~228), change:

```rust
        let ldpc_decoder = LdpcDecoder::new(config.ldpc_iterations)?;
```

to:

```rust
        let ldpc_decoder = LdpcDecoder::new_with_osd(
            config.ldpc_iterations,
            config.osd_depth.map(|d| OsdConfig { max_depth: d }),
        )?;
```

- [ ] **Step 6: Run tests to verify**

Run: `cargo test -p pancetta-ft8 --lib`
Expected: All existing tests pass + new `test_ldpc_decode_soft_with_osd_fallback` passes

- [ ] **Step 7: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat(osd): integrate OSD fallback into LDPC decoder pipeline"
```

---

### Task 4: Add integration tests for OSD weak-signal recovery

**Files:**
- Create: `pancetta-ft8/tests/osd_tests.rs`

These tests require the `transmit` feature to access the encoder and modulator for generating test signals.

- [ ] **Step 1: Write weak-signal OSD recovery test**

Create `pancetta-ft8/tests/osd_tests.rs`:

```rust
//! Integration tests for OSD (Ordered Statistics Decoding).
//!
//! These tests verify that OSD recovers signals too weak for BP alone,
//! using the full encode → modulate → decode pipeline.

#![cfg(feature = "transmit")]

use pancetta_ft8::{Ft8Config, Ft8Decoder, Ft8Encoder, Ft8EncodingConfig, Ft8Modulator, ModulatorConfig};
use pancetta_ft8::protocol::Protocol;

/// Helper: encode and modulate a message, return audio samples as f64
fn generate_ft8_signal(message: &str, frequency_hz: f64, amplitude: f64) -> Vec<f64> {
    let encoder = Ft8Encoder::new(Ft8EncodingConfig::default());
    let symbols = encoder.encode_message(message).expect("encoding should succeed");
    let modulator = Ft8Modulator::new(ModulatorConfig::default());
    let samples_f32 = modulator.modulate(&symbols, frequency_hz as f32);

    samples_f32.iter().map(|&s| s as f64 * amplitude).collect()
}

/// Add white Gaussian noise to a signal at a given SNR (dB) relative to signal power.
fn add_noise(signal: &[f64], snr_db: f64) -> Vec<f64> {
    use std::f64::consts::PI;

    let signal_power: f64 = signal.iter().map(|s| s * s).sum::<f64>() / signal.len() as f64;
    let noise_power = signal_power / 10.0f64.powf(snr_db / 10.0);
    let noise_std = noise_power.sqrt();

    // Simple Box-Muller noise generation with fixed seed for reproducibility
    let mut noisy = signal.to_vec();
    let mut seed: u64 = 42;
    for i in (0..noisy.len()).step_by(2) {
        // LCG PRNG
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let u1 = (seed >> 33) as f64 / (1u64 << 31) as f64;
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let u2 = (seed >> 33) as f64 / (1u64 << 31) as f64;

        let u1 = u1.max(1e-10); // avoid log(0)
        let z0 = (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).cos();
        let z1 = (-2.0 * u1.ln()).sqrt() * (2.0 * PI * u2).sin();

        noisy[i] += z0 * noise_std;
        if i + 1 < noisy.len() {
            noisy[i + 1] += z1 * noise_std;
        }
    }
    noisy
}

/// Test that OSD recovers a weak signal that BP alone cannot decode.
///
/// Strategy: generate a signal at low SNR, decode with OSD disabled (should fail)
/// and with OSD enabled (should succeed). The SNR is tuned so BP fails but OSD-2
/// can recover — this is the ~2 dB window where OSD provides value.
#[test]
fn test_osd_recovers_weak_signal() {
    let message = "CQ W1ABC FN42";
    let freq = 1500.0;

    // Try a range of SNRs to find one where BP fails but OSD succeeds.
    // This makes the test robust against minor decoder changes.
    let mut found_osd_advantage = false;

    for snr_db in [-22.0, -21.0, -20.0, -19.0, -18.0] {
        let signal = generate_ft8_signal(message, freq, 0.1);
        let noisy = add_noise(&signal, snr_db);

        // Pad to window size
        let window_samples = pancetta_ft8::WINDOW_SAMPLES;
        let mut buffer = vec![0.0f32; window_samples];
        for (i, &s) in noisy.iter().enumerate().take(window_samples) {
            buffer[i] = s as f32;
        }

        // Decode WITHOUT OSD
        let mut config_no_osd = Ft8Config::default();
        config_no_osd.osd_depth = None;
        config_no_osd.max_decode_passes = 1;
        let mut decoder_no_osd = Ft8Decoder::new(config_no_osd).unwrap();
        let results_no_osd = decoder_no_osd.decode_window(&buffer).unwrap_or_default();
        let found_no_osd = results_no_osd.iter().any(|m| m.text.contains("W1ABC"));

        // Decode WITH OSD
        let mut config_osd = Ft8Config::default();
        config_osd.osd_depth = Some(2);
        config_osd.max_decode_passes = 1;
        let mut decoder_osd = Ft8Decoder::new(config_osd).unwrap();
        let results_osd = decoder_osd.decode_window(&buffer).unwrap_or_default();
        let found_osd = results_osd.iter().any(|m| m.text.contains("W1ABC"));

        if !found_no_osd && found_osd {
            found_osd_advantage = true;
            break;
        }
    }

    assert!(
        found_osd_advantage,
        "OSD should recover at least one SNR level where BP alone fails"
    );
}

/// Verify that OSD does not produce false positives on pure noise.
#[test]
fn test_osd_no_false_positives_on_noise() {
    let window_samples = pancetta_ft8::WINDOW_SAMPLES;

    // Generate pure noise (no signal)
    let noise_signal = vec![0.0; window_samples];
    let noisy = add_noise(&noise_signal, 0.0); // just noise

    let mut buffer = vec![0.0f32; window_samples];
    for (i, &s) in noisy.iter().enumerate().take(window_samples) {
        buffer[i] = s as f32;
    }

    let mut config = Ft8Config::default();
    config.osd_depth = Some(2);
    config.max_decode_passes = 1;
    let mut decoder = Ft8Decoder::new(config).unwrap();
    let results = decoder.decode_window(&buffer).unwrap_or_default();

    // Filter out empty/unknown messages
    let real_decodes: Vec<_> = results
        .iter()
        .filter(|m| !m.text.is_empty() && m.text != "<Unknown>")
        .collect();

    assert!(
        real_decodes.is_empty(),
        "OSD should not produce false decodes on pure noise, got: {:?}",
        real_decodes.iter().map(|m| &m.text).collect::<Vec<_>>()
    );
}
```

- [ ] **Step 2: Run the integration tests**

Run: `cargo test -p pancetta-ft8 --features transmit --test osd_tests -- --nocapture`
Expected: Both tests pass

- [ ] **Step 3: Commit**

```bash
git add pancetta-ft8/tests/osd_tests.rs
git commit -m "test(osd): add integration tests for weak-signal recovery and false positive check"
```

---

### Task 5: Run benchmark and record results

**Files:**
- Modify: `benchmarks/BASELINE.md`

- [ ] **Step 1: Run benchmark with OSD enabled**

Run: `cargo run --release --features benchmark -- benchmark-decode tests/fixtures/wav/`
Capture the output showing per-file decode counts for both Pancetta and ft8_lib.

- [ ] **Step 2: Run benchmark with OSD disabled for comparison**

Temporarily set `osd_depth: None` in `Ft8Config::default()`, rebuild, and run the same benchmark.
Record the OSD-off vs OSD-on difference.

(Alternatively, if the benchmark CLI accepts a flag, use that.)

- [ ] **Step 3: Update BASELINE.md**

Append a new section to `benchmarks/BASELINE.md`:

```markdown
---

## Results (post-OSD implementation)

### Date: 2026-03-30

[Insert actual benchmark output here]

### Improvements Applied

1. **OSD-2 fallback** — ordered statistics decoding depth 2 (4,187 trials) on BP failures

### Improvement Summary

| Metric | Before OSD | After OSD | Change |
|--------|-----------|-----------|--------|
| Pancetta decodes | 50 | [N] | [+X%] |
| ft8_lib-only | [N] | [N] | [reduced by Y] |
| Parity % | [N] | [N] | [+Z%] |
```

- [ ] **Step 4: Commit**

```bash
git add benchmarks/BASELINE.md
git commit -m "docs: record post-OSD benchmark results"
```

---

### Task 6: Run full test suite and fix any regressions

**Files:**
- Any files needing fixes

- [ ] **Step 1: Run full test suite**

Run: `cargo test --features transmit -p pancetta-ft8`
Expected: All tests pass (existing 186+ new OSD tests)

- [ ] **Step 2: Run clippy**

Run: `cargo clippy -p pancetta-ft8 --features transmit -- -D warnings`
Expected: No warnings

- [ ] **Step 3: Fix any issues found**

If tests fail or clippy warns, fix the issues.

- [ ] **Step 4: Commit fixes if any**

```bash
git add -A
git commit -m "fix: address test/clippy issues from OSD integration"
```
