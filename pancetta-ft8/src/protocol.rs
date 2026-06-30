//! Protocol abstraction layer for FT8, FT4, and FT2 digital modes.
//!
//! All three protocols share the same 77-bit payload, LDPC(174,91), and CRC-14.
//! They differ in modulation, timing, sync structure, and cycle length.
//! This module defines the parameters that distinguish them.

use std::ops::Range;

/// Supported digital mode protocols
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Protocol {
    Ft8,
    Ft4,
    #[cfg(feature = "ft2")]
    Ft2,
}

impl Protocol {
    /// Slot length in nanoseconds for this protocol.
    ///
    /// Convenience over `ProtocolParams::{ft8,ft4,ft2}().slot_ns()`.
    pub fn slot_ns(self) -> i64 {
        match self {
            Protocol::Ft8 => ProtocolParams::ft8().slot_ns(),
            Protocol::Ft4 => ProtocolParams::ft4().slot_ns(),
            #[cfg(feature = "ft2")]
            Protocol::Ft2 => ProtocolParams::ft2().slot_ns(),
        }
    }
}

impl std::fmt::Display for Protocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Protocol::Ft8 => write!(f, "FT8"),
            Protocol::Ft4 => write!(f, "FT4"),
            #[cfg(feature = "ft2")]
            Protocol::Ft2 => write!(f, "FT2"),
        }
    }
}

/// Modulation type used by a protocol
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ModulationType {
    /// Pure continuous-phase FSK (no Gaussian smoothing).
    ///
    /// NOTE (Phase D audit 2026-06-02): formerly used as the FT8 tag.
    /// FT8 is canonically **8-GFSK with BT=2.0** per Franke-Taylor QEX
    /// 2020; pancetta's modulator (`modulator.rs`) already produces
    /// GFSK output, so the tag was the only thing wrong. FT8 now
    /// correctly uses `Gfsk { bt: 2.0 }`. This variant is retained
    /// for completeness; no current production protocol references it.
    /// See `docs/engineering/2026-06-02-engineering-substance-audit.md`
    /// (claim 4).
    Cpfsk,
    /// Gaussian FSK with specified bandwidth-time product.
    /// FT8 uses `Gfsk { bt: 2.0 }`; FT4 uses `Gfsk { bt: 1.0 }`.
    Gfsk { bt: f64 },
}

/// Complete set of protocol parameters that distinguish FT8/FT4/FT2.
///
/// All derived values (samples_per_symbol, tx_duration, etc.) are computed
/// from the base parameters and a given sample rate.
#[derive(Debug, Clone)]
pub struct ProtocolParams {
    /// Which protocol
    pub protocol: Protocol,
    /// Number of FSK tones (8 for FT8/FT2, 4 for FT4)
    pub num_tones: usize,
    /// Bits per symbol (3 for 8-FSK, 2 for 4-FSK)
    pub bits_per_symbol: usize,
    /// Total symbols in one transmission
    pub num_symbols: usize,
    /// Symbol period in seconds
    pub symbol_period: f64,
    /// Tone spacing in Hz
    pub tone_spacing: f64,
    /// Costas synchronization arrays (one per sync group).
    /// FT8: all three groups use the same 7-element array.
    /// FT4: four groups each use a different 4-element array.
    pub costas_arrays: &'static [&'static [u8]],
    /// Symbol positions where each Costas array starts
    pub costas_positions: &'static [usize],
    /// Length of each Costas array insertion
    pub costas_length: usize,
    /// Ranges of data symbol positions (between/around sync arrays)
    pub data_symbol_ranges: &'static [Range<usize>],
    /// Number of data symbols (sum of data_symbol_ranges lengths)
    pub num_data_symbols: usize,
    /// Cycle duration in seconds
    pub cycle_duration: f64,
    /// Modulation type
    pub modulation: ModulationType,
    /// XOR scrambling sequence applied to payload before CRC (FT4 only, None for FT8)
    pub xor_sequence: Option<&'static [u8; 10]>,
}

// ============================================================================
// FT8 constants
// ============================================================================

/// FT8 Costas synchronization array (same for all 3 sync groups)
pub const FT8_COSTAS: [u8; 7] = [3, 1, 4, 0, 6, 5, 2];

/// FT8 Costas arrays (all three groups use the same pattern)
static FT8_COSTAS_ARRAYS: [&[u8]; 3] = [&FT8_COSTAS, &FT8_COSTAS, &FT8_COSTAS];

/// FT8 Costas array positions (symbol indices where sync arrays start)
static FT8_COSTAS_POSITIONS: [usize; 3] = [0, 36, 72];

/// FT8 data symbol ranges: positions 7..36 and 43..72 (29+29 = 58 data symbols)
static FT8_DATA_RANGES: [Range<usize>; 2] = [7..36, 43..72];

// ============================================================================
// FT4 constants (from WSJT-X ft8_lib/constants.c — verified against kgoba/ft8_lib)
// ============================================================================

/// FT4 uses four distinct Costas-like sync arrays (from kFT4_Costas_pattern)
pub const FT4_COSTAS_0: [u8; 4] = [0, 1, 3, 2];
pub const FT4_COSTAS_1: [u8; 4] = [1, 0, 2, 3];
pub const FT4_COSTAS_2: [u8; 4] = [2, 3, 1, 0];
pub const FT4_COSTAS_3: [u8; 4] = [3, 2, 0, 1];

/// FT4 Costas arrays (four different patterns)
static FT4_COSTAS_ARRAYS: [&[u8]; 4] = [&FT4_COSTAS_0, &FT4_COSTAS_1, &FT4_COSTAS_2, &FT4_COSTAS_3];

/// FT4 sync positions: 1-4, 34-37, 67-70, 100-103
/// Full layout: R S4₀ D29 S4₁ D29 S4₂ D29 S4₃ R
///   [0] = ramp, [1..5] = sync0, [5..34] = data, [34..38] = sync1,
///   [38..67] = data, [67..71] = sync2, [71..100] = data, [100..104] = sync3, [104] = ramp
static FT4_COSTAS_POSITIONS: [usize; 4] = [1, 34, 67, 100];

/// FT4 data symbol ranges: 5..34, 38..67, 71..100 (29×3 = 87 data symbols)
static FT4_DATA_RANGES: [Range<usize>; 3] = [5..34, 38..67, 71..100];

/// FT4 XOR scrambling sequence (applied to payload before CRC)
pub const FT4_XOR_SEQUENCE: [u8; 10] = [0x4A, 0x5E, 0x89, 0xB4, 0xB0, 0x8A, 0x79, 0x55, 0xBE, 0x28];

// ============================================================================
// FT2 constants (experimental — Decodium layout)
// ============================================================================

#[cfg(feature = "ft2")]
/// FT2 Costas arrays (same as FT8 — 8-GFSK, same sync structure)
static FT2_COSTAS_ARRAYS: [&[u8]; 3] = [&FT8_COSTAS, &FT8_COSTAS, &FT8_COSTAS];

#[cfg(feature = "ft2")]
/// FT2 Costas positions (same structure as FT8)
static FT2_COSTAS_POSITIONS: [usize; 3] = [0, 36, 72];

#[cfg(feature = "ft2")]
/// FT2 data symbol ranges (same as FT8)
static FT2_DATA_RANGES: [Range<usize>; 2] = [7..36, 43..72];

// ============================================================================
// Protocol parameter constructors
// ============================================================================

impl ProtocolParams {
    /// FT8 protocol parameters
    ///
    /// 8-CPFSK, 79 symbols, 0.16s symbol period, 6.25 Hz spacing, 15s cycle
    pub fn ft8() -> Self {
        Self {
            protocol: Protocol::Ft8,
            num_tones: 8,
            bits_per_symbol: 3,
            num_symbols: 79,
            symbol_period: 0.16,
            tone_spacing: 6.25,
            costas_arrays: &FT8_COSTAS_ARRAYS,
            costas_positions: &FT8_COSTAS_POSITIONS,
            costas_length: 7,
            data_symbol_ranges: &FT8_DATA_RANGES,
            num_data_symbols: 58, // 29 + 29
            cycle_duration: 15.0,
            // FT8 is 8-GFSK BT=2.0 per Franke-Taylor QEX 2020. The
            // modulator (modulator.rs) already produces GFSK output with
            // BT=2.0; only the enum tag was previously wrong (Cpfsk).
            // Fix landed 2026-06-02 (Phase C) per
            // docs/engineering/2026-06-02-engineering-substance-audit.md.
            modulation: ModulationType::Gfsk { bt: 2.0 },
            xor_sequence: None,
        }
    }

    /// FT4 protocol parameters (from WSJT-X / kgoba/ft8_lib)
    ///
    /// 4-GFSK BT=1.0, 105 symbols, 0.048s symbol period, 20.833 Hz spacing, 7.5s cycle
    /// Layout: R S4₀ D29 S4₁ D29 S4₂ D29 S4₃ R (87 data symbols, 4×4 sync, 2 ramp)
    pub fn ft4() -> Self {
        Self {
            protocol: Protocol::Ft4,
            num_tones: 4,
            bits_per_symbol: 2,
            num_symbols: 105,
            symbol_period: 0.048,
            tone_spacing: 20.8333,
            costas_arrays: &FT4_COSTAS_ARRAYS,
            costas_positions: &FT4_COSTAS_POSITIONS,
            costas_length: 4,
            data_symbol_ranges: &FT4_DATA_RANGES,
            num_data_symbols: 87, // 29 × 3
            cycle_duration: 7.5,
            modulation: ModulationType::Gfsk { bt: 1.0 },
            xor_sequence: Some(&FT4_XOR_SEQUENCE),
        }
    }

    /// FT2 protocol parameters (experimental)
    ///
    /// 8-GFSK BT=2.0, 79 symbols, ~0.040s symbol period, ~25 Hz spacing, ~3.2s cycle
    /// WARNING: FT2 is experimental. Two incompatible specifications exist.
    /// This uses the Decodium layout.
    #[cfg(feature = "ft2")]
    pub fn ft2() -> Self {
        Self {
            protocol: Protocol::Ft2,
            num_tones: 8,
            bits_per_symbol: 3,
            num_symbols: 79,
            symbol_period: 0.040,
            tone_spacing: 25.0,
            costas_arrays: &FT2_COSTAS_ARRAYS,
            costas_positions: &FT2_COSTAS_POSITIONS,
            costas_length: 7,
            data_symbol_ranges: &FT2_DATA_RANGES,
            num_data_symbols: 58,
            cycle_duration: 3.2,
            modulation: ModulationType::Gfsk { bt: 2.0 },
            xor_sequence: None,
        }
    }

    // ========================================================================
    // Derived parameters
    // ========================================================================

    /// Slot length in nanoseconds (`cycle_duration` converted to ns).
    ///
    /// FT8 = 15_000_000_000, FT4 = 7_500_000_000. This is the period to feed
    /// the period-generic slot-timing helpers in `pancetta_core::slot`
    /// (`*_with_period` variants); the bare FT8 wrappers there hardcode
    /// `SLOT_NS == ft8().slot_ns()`.
    pub fn slot_ns(&self) -> i64 {
        (self.cycle_duration * 1e9) as i64
    }

    /// Samples per symbol at the given sample rate
    pub fn samples_per_symbol(&self, sample_rate: u32) -> usize {
        (self.symbol_period * sample_rate as f64) as usize
    }

    /// Total transmission duration in seconds
    pub fn tx_duration(&self) -> f64 {
        self.num_symbols as f64 * self.symbol_period
    }

    /// Total samples in a complete transmission at the given sample rate
    pub fn total_samples(&self, sample_rate: u32) -> usize {
        (self.tx_duration() * sample_rate as f64) as usize
    }

    /// Total samples in a full receive window at the given sample rate
    pub fn window_samples(&self, sample_rate: u32) -> usize {
        (self.cycle_duration * sample_rate as f64) as usize
    }

    /// FFT size for spectrogram (samples_per_symbol * freq_osr)
    pub fn spec_nfft(&self, sample_rate: u32, freq_osr: usize) -> usize {
        self.samples_per_symbol(sample_rate) * freq_osr
    }

    /// Spectrogram step size (samples_per_symbol / time_osr)
    pub fn spec_step(&self, sample_rate: u32, time_osr: usize) -> usize {
        self.samples_per_symbol(sample_rate) / time_osr
    }

    /// Minimum bandwidth of one signal (num_tones * tone_spacing)
    pub fn signal_bandwidth(&self) -> f64 {
        self.num_tones as f64 * self.tone_spacing
    }

    /// Check if a symbol index is a sync (Costas) position
    pub fn is_sync_symbol(&self, idx: usize) -> bool {
        for &pos in self.costas_positions {
            if idx >= pos && idx < pos + self.costas_length {
                return true;
            }
        }
        false
    }

    /// Get the Costas array value for a sync symbol position.
    /// Returns None if `idx` is not a sync position.
    pub fn costas_value(&self, idx: usize) -> Option<u8> {
        for (group, &pos) in self.costas_positions.iter().enumerate() {
            if idx >= pos && idx < pos + self.costas_length {
                return Some(self.costas_arrays[group][idx - pos]);
            }
        }
        None
    }

    /// Check if a symbol index is a data position
    pub fn is_data_symbol(&self, idx: usize) -> bool {
        self.data_symbol_ranges.iter().any(|r| r.contains(&idx))
    }

    /// All data symbol indices, flattened from `data_symbol_ranges`.
    ///
    /// Perf (Pass 1b / A2): read once per candidate in several hot decode
    /// loops; it used to allocate + collect a fresh `Vec` on every call
    /// (thousands of allocations per window on a busy band). The indices are a
    /// pure function of the protocol's `&'static` ranges, so they are computed
    /// once per protocol into a `OnceLock` and returned as a `&'static` slice.
    /// Bit-identical to the previous flatten — only the allocation is removed.
    pub fn data_symbol_indices(&self) -> &'static [usize] {
        use std::sync::OnceLock;
        fn compute(ranges: &[Range<usize>]) -> Vec<usize> {
            ranges.iter().flat_map(|r| r.clone()).collect()
        }
        match self.protocol {
            Protocol::Ft8 => {
                static C: OnceLock<Vec<usize>> = OnceLock::new();
                C.get_or_init(|| compute(&FT8_DATA_RANGES)).as_slice()
            }
            Protocol::Ft4 => {
                static C: OnceLock<Vec<usize>> = OnceLock::new();
                C.get_or_init(|| compute(&FT4_DATA_RANGES)).as_slice()
            }
            #[cfg(feature = "ft2")]
            Protocol::Ft2 => {
                static C: OnceLock<Vec<usize>> = OnceLock::new();
                C.get_or_init(|| compute(&FT2_DATA_RANGES)).as_slice()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ft8_params() {
        let p = ProtocolParams::ft8();
        assert_eq!(p.num_tones, 8);
        assert_eq!(p.num_symbols, 79);
        assert_eq!(p.bits_per_symbol, 3);
        assert_eq!(p.symbol_period, 0.16);
        assert_eq!(p.tone_spacing, 6.25);
        assert_eq!(p.costas_arrays[0], &[3, 1, 4, 0, 6, 5, 2]);
        assert_eq!(p.costas_arrays[1], &[3, 1, 4, 0, 6, 5, 2]); // all same for FT8
        assert_eq!(p.costas_positions, &[0, 36, 72]);
        assert_eq!(p.num_data_symbols, 58);
        assert_eq!(p.cycle_duration, 15.0);
        assert!(p.xor_sequence.is_none());

        // Derived
        assert_eq!(p.samples_per_symbol(12000), 1920);
        assert_eq!(p.total_samples(12000), 151680);
        assert!((p.tx_duration() - 12.64).abs() < 0.001);
        assert_eq!(p.signal_bandwidth(), 50.0);
    }

    #[test]
    fn test_ft4_params() {
        let p = ProtocolParams::ft4();
        assert_eq!(p.num_tones, 4);
        assert_eq!(p.num_symbols, 105);
        assert_eq!(p.bits_per_symbol, 2);
        assert_eq!(p.tone_spacing, 20.8333);
        // FT4 uses 4 different Costas patterns
        assert_eq!(p.costas_arrays[0], &[0, 1, 3, 2]);
        assert_eq!(p.costas_arrays[1], &[1, 0, 2, 3]);
        assert_eq!(p.costas_arrays[2], &[2, 3, 1, 0]);
        assert_eq!(p.costas_arrays[3], &[3, 2, 0, 1]);
        assert_eq!(p.costas_positions, &[1, 34, 67, 100]);
        assert_eq!(p.num_data_symbols, 87); // 29 × 3
        assert_eq!(p.cycle_duration, 7.5);
        assert!(p.xor_sequence.is_some());

        // FT4 samples per symbol at 12kHz: 0.048 * 12000 = 576
        assert_eq!(p.samples_per_symbol(12000), 576);
        // Total samples: 105 * 576 = 60480
        assert_eq!(p.total_samples(12000), 60480);
    }

    #[cfg(feature = "ft2")]
    #[test]
    fn test_ft2_params() {
        let p = ProtocolParams::ft2();
        assert_eq!(p.num_tones, 8);
        assert_eq!(p.num_symbols, 79);
        assert_eq!(p.bits_per_symbol, 3);
        assert_eq!(p.num_data_symbols, 58);
        assert_eq!(p.cycle_duration, 3.2);
    }

    #[test]
    fn test_ft8_symbol_classification() {
        let p = ProtocolParams::ft8();

        // Costas positions
        assert!(p.is_sync_symbol(0));
        assert!(p.is_sync_symbol(6));
        assert!(p.is_sync_symbol(36));
        assert!(p.is_sync_symbol(72));
        assert!(p.is_sync_symbol(78));

        // Data positions
        assert!(p.is_data_symbol(7));
        assert!(p.is_data_symbol(35));
        assert!(p.is_data_symbol(43));
        assert!(p.is_data_symbol(71));

        // Neither (boundary)
        assert!(!p.is_data_symbol(0));
        assert!(!p.is_data_symbol(36));
        assert!(!p.is_sync_symbol(7));
        assert!(!p.is_sync_symbol(35));
    }

    #[test]
    fn test_ft8_data_indices() {
        let p = ProtocolParams::ft8();
        let indices = p.data_symbol_indices();
        assert_eq!(indices.len(), 58);
        assert_eq!(indices[0], 7);
        assert_eq!(indices[29], 43);
        assert_eq!(indices[57], 71);
    }

    #[test]
    fn test_ft4_data_indices() {
        let p = ProtocolParams::ft4();
        let indices = p.data_symbol_indices();
        assert_eq!(indices.len(), 87);
    }

    #[test]
    fn test_ft8_backward_compatible_constants() {
        let p = ProtocolParams::ft8();
        assert_eq!(p.num_tones, crate::NUM_TONES);
        assert_eq!(p.num_symbols, crate::NUM_SYMBOLS);
        assert_eq!(p.tone_spacing, crate::TONE_SPACING);
        assert_eq!(p.symbol_period, crate::SYMBOL_DURATION);
        assert!((p.tx_duration() - crate::MESSAGE_DURATION).abs() < 0.001);
    }

    #[test]
    fn test_ft4_costas_values() {
        let p = ProtocolParams::ft4();
        // Sync group 0 at position 1
        assert_eq!(p.costas_value(1), Some(0));
        assert_eq!(p.costas_value(2), Some(1));
        assert_eq!(p.costas_value(3), Some(3));
        assert_eq!(p.costas_value(4), Some(2));
        // Sync group 1 at position 34
        assert_eq!(p.costas_value(34), Some(1));
        assert_eq!(p.costas_value(35), Some(0));
        // Data position
        assert_eq!(p.costas_value(5), None);
        // Ramp position
        assert_eq!(p.costas_value(0), None);
    }

    #[test]
    fn test_ft4_data_bit_count() {
        let p = ProtocolParams::ft4();
        // 87 data symbols × 2 bits/symbol = 174 LDPC codeword bits
        assert_eq!(p.num_data_symbols * p.bits_per_symbol, 174);
    }

    #[test]
    fn test_ft8_data_bit_count() {
        let p = ProtocolParams::ft8();
        // 58 data symbols × 3 bits/symbol = 174 LDPC codeword bits
        assert_eq!(p.num_data_symbols * p.bits_per_symbol, 174);
    }

    #[test]
    fn test_protocol_display() {
        assert_eq!(Protocol::Ft8.to_string(), "FT8");
        assert_eq!(Protocol::Ft4.to_string(), "FT4");
    }

    #[test]
    fn test_slot_ns() {
        assert_eq!(ProtocolParams::ft8().slot_ns(), 15_000_000_000);
        assert_eq!(ProtocolParams::ft4().slot_ns(), 7_500_000_000);
        // Protocol convenience matches ProtocolParams.
        assert_eq!(Protocol::Ft8.slot_ns(), 15_000_000_000);
        assert_eq!(Protocol::Ft4.slot_ns(), 7_500_000_000);
    }

    #[cfg(feature = "ft2")]
    #[test]
    fn test_slot_ns_ft2() {
        let p = ProtocolParams::ft2();
        assert_eq!(p.slot_ns(), (p.cycle_duration * 1e9) as i64);
        assert_eq!(Protocol::Ft2.slot_ns(), p.slot_ns());
    }

    #[test]
    fn test_spec_nfft() {
        let p = ProtocolParams::ft8();
        // FT8: 1920 * 2 = 3840
        assert_eq!(p.spec_nfft(12000, 2), 3840);

        let p4 = ProtocolParams::ft4();
        // FT4: 576 * 2 = 1152
        assert_eq!(p4.spec_nfft(12000, 2), 1152);
    }
}
