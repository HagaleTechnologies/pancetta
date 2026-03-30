# OSD (Ordered Statistics Decoding) for FT8 LDPC(174,91)

**Date:** 2026-03-30
**Goal:** Add OSD-0/1/2 as a fallback when LDPC belief propagation fails, recovering weak signals that BP alone cannot decode — targeting ~+2 dB sensitivity improvement.

**Parent spec:** [WSJT-X Parity Design](2026-03-30-wsjt-x-parity-design.md) — Phase 3

## Context

Pancetta's FT8 decoder currently uses min-sum belief propagation (BP) for LDPC decoding. When BP fails to converge after 100 iterations, the candidate is discarded. WSJT-X and ft8_lib use Ordered Statistics Decoding (OSD) as a fallback, recovering an additional ~2 dB of weak signals.

The benchmark baseline shows 9 ft8_lib-only decodes that Pancetta misses — many of these are likely weak signals where BP fails but OSD would succeed.

## Algorithm

OSD exploits the structure of the LDPC(174,91) code by sorting bits by reliability and solving a linear system over GF(2).

### Step-by-step

1. **Rank bits by reliability:** Sort all 174 bit positions by descending |LLR| from BP's final output. The most reliable bits (highest |LLR|) come first.

2. **Permute the generator matrix:** Reorder the columns of the 91×174 generator matrix to match the reliability ranking.

3. **Gaussian elimination:** Row-reduce the permuted generator matrix so the leftmost 91 columns form an identity matrix. If the matrix is singular (rank < 91), the decode attempt fails — this is rare in practice.

4. **OSD-0 (order 0):** Hard-decide the 91 most-reliable bits using LLR signs. Compute the 83 parity bits via the reduced generator matrix: `parity[i] = dot(info_bits, G_reduced[i]) mod 2`. Un-permute the 174-bit codeword back to original column order. Extract the first 91 bits (info bits in original order) and check CRC-14. If valid, accept.

5. **OSD-1 (order 1):** For each of the 91 info bit positions, flip that bit from the OSD-0 solution, recompute the 83 parity bits, check CRC-14. 91 trials total.

6. **OSD-2 (order 2):** For each pair of info bit positions (i, j) where i < j, flip both bits from OSD-0, recompute parity, check CRC-14. C(91,2) = 4,095 trials.

7. **Accept first valid:** Return the first codeword that passes CRC-14, trying OSD-0 first, then OSD-1, then OSD-2. If none pass, OSD fails and the candidate is discarded.

### Complexity

- Gaussian elimination: O(91² × 22) byte operations — done once per candidate
- Per trial: XOR of ~45 packed byte rows (22 bytes each) + CRC-14 check — microseconds
- Total trials: 1 + 91 + 4,095 = 4,187 per OSD invocation
- OSD only runs on BP failures (minority of candidates), so total cost is bounded

## Architecture

### New module: `pancetta-ft8/src/osd.rs`

```rust
/// Configuration for OSD depth.
pub struct OsdConfig {
    /// Maximum OSD order: 0, 1, or 2. Default: 2.
    pub max_depth: u8,
}

impl Default for OsdConfig {
    fn default() -> Self {
        Self { max_depth: 2 }
    }
}

/// Ordered Statistics Decoder for the FT8 LDPC(174,91) code.
pub struct OsdDecoder {
    config: OsdConfig,
    /// Systematic generator matrix in packed binary: 91 rows × 22 bytes (174 bits).
    /// Row i represents the full 174-bit codeword for info bit i set to 1.
    /// Constructed at init: first 91 columns = I_91, last 83 columns = parity
    /// derived from ldpc::LDPC_GENERATOR.
    generator: [[u8; 22]; 91],
}

impl OsdDecoder {
    pub fn new(config: OsdConfig) -> Self;

    /// Attempt OSD decode given the channel LLRs from BP.
    ///
    /// Returns the decoded 174-bit codeword if a valid codeword (CRC-14 pass)
    /// is found at any depth up to `max_depth`. Returns `None` otherwise.
    pub fn decode(&self, llrs: &[f32; 174]) -> Option<BitVec>;
}
```

### Internal helpers (private)

- `sort_by_reliability(llrs) -> (permutation, sorted_llrs)` — returns index permutation and sorted |LLR| values
- `permute_generator(generator, perm) -> permuted_generator` — reorder columns of the generator matrix
- `gaussian_eliminate(matrix) -> Option<reduced_matrix>` — row-reduce to systematic form; returns None if singular
- `solve_osd0(sorted_llrs, reduced_matrix) -> [u8; 22]` — hard-decide info bits, compute parity
- `unpermute_codeword(codeword, perm) -> [u8; 22]` — reverse column permutation
- `check_crc14(codeword) -> bool` — extract first 91 bits, verify CRC-14

### Matrix representation

Each row of the generator/parity matrix is stored as `[u8; 22]` (174 bits packed into 22 bytes, MSB first). GF(2) row operations (XOR, swap) operate on entire 22-byte rows. This is cache-friendly and avoids bit-level addressing for row operations.

For trial solutions, parity recomputation uses XOR: flipping info bit `k` toggles all parity bits where `G_reduced[i][k] == 1`. This is a single conditional XOR of the packed row, making each trial O(22) bytes of work.

### Integration into decoder

The `LdpcDecoder` struct gains an optional `OsdDecoder` field:

```rust
struct LdpcDecoder {
    max_iterations: usize,
    parity_check_matrix: ParityCheckMatrix,
    var_positions: Vec<Vec<(usize, usize)>>,
    normalization_factor: f32,
    osd: Option<OsdDecoder>,  // NEW
}
```

`decode_soft` changes to:

1. Run BP as before
2. Check syndrome on BP output
3. If syndrome is zero → return hard-decided bits (existing behavior)
4. If syndrome is non-zero AND `self.osd` is Some → call `osd.decode(&bp_llrs)`
5. If OSD returns Some(codeword) → return those bits
6. Otherwise → return the BP output as-is (caller's CRC check will reject)

The caller in `decode_candidate` (line 844) doesn't change — it still checks CRC on whatever `decode_soft` returns.

### Ft8Config extension

```rust
pub struct Ft8Config {
    // ... existing fields ...
    /// OSD depth (0, 1, or 2). Set to None to disable OSD. Default: Some(2).
    pub osd_depth: Option<u8>,
}
```

## Testing

### Unit tests (in `osd.rs`)

1. **Gaussian elimination correctness** — construct a small known matrix, verify it reduces to systematic form
2. **Permutation round-trip** — permute and un-permute a codeword, verify identity
3. **OSD-0 on clean codeword** — encode a known message, provide perfect LLRs with correct signs, verify OSD-0 recovers it
4. **OSD-1 on 1-bit error** — flip 1 bit in a valid codeword's LLRs (make one bit unreliable), verify OSD-1 recovers
5. **OSD-2 on 2-bit error** — flip 2 bits, verify OSD-2 recovers
6. **Singular matrix handling** — craft degenerate LLRs that cause rank deficiency, verify graceful None return

### Integration tests (in `tests/`)

7. **Weak-signal decode** — encode a message, modulate at SNR ~-20 dB (below BP threshold), verify OSD recovers where BP alone fails
8. **No false positives** — pure noise input, verify OSD does not produce spurious CRC-passing decodes

### Benchmark regression

9. Re-run `cargo run --release -- benchmark-decode` on the 12-file test corpus. Compare:
   - Total Pancetta decodes (expect increase)
   - ft8_lib-only count (expect decrease from 9)
   - Parity percentage (expect increase from current 131.6% ratio)

## Design Decisions

| Decision | Choice | Rationale |
|---|---|---|
| Default depth | OSD-2 | Matches WSJT-X deep search, maximizes sensitivity |
| Search width | All 91 info bits | Maximum recovery, matches WSJT-X |
| BP failure gating | None — run OSD on all BP failures | OSD is cheap (GF(2) XOR), avoid filtering recoverable signals |
| Matrix storage | `[u8; 22]` packed rows | Cache-friendly, fast XOR, avoids bitvec overhead in hot loop |
| CRC check location | Inside OSD (per trial) | Early termination on first valid codeword |
| Integration | Optional field in LdpcDecoder | Zero-cost when disabled, no API change for callers |
| Configurability | `OsdConfig { max_depth }` | Future knob, default 2 |

## Files

| File | Action | Purpose |
|---|---|---|
| `pancetta-ft8/src/osd.rs` | Create | OSD decoder implementation |
| `pancetta-ft8/src/lib.rs` | Modify | Add `pub mod osd;` |
| `pancetta-ft8/src/decoder.rs` | Modify | Add OSD field to LdpcDecoder, call OSD on BP failure |
| `pancetta-ft8/src/decoder.rs` | Modify | Add `osd_depth` to Ft8Config |
| `pancetta-ft8/tests/osd_tests.rs` | Create | Integration tests for OSD |
| `benchmarks/BASELINE.md` | Modify | Record post-OSD benchmark results |

## Out of Scope

- AP (a priori) decoding — separate phase, uses OSD infrastructure but adds LLR priming
- OSD-3+ — diminishing returns, not used by WSJT-X
- Parallel OSD trials — single-threaded is fast enough given bounded trial count
- Custom generator matrix — always uses LDPC(174,91) from ldpc.rs
