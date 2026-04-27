# Multi-Protocol + Multi-QSO Implementation Plan

## Architecture Insight

FT8, FT4, and FT2 share the same 77-bit payload, LDPC(174,91), and CRC-14. They differ only in modulation, timing, sync structure, and cycle length. The encoder, LDPC, and message parser are fully reusable. The decoder and modulator need parameterization.

---

## Phase 1: Protocol Abstraction Layer

Refactor DSP core from hardcoded FT8 constants to a `ProtocolParams` struct. No new protocols yet — all 186 existing tests must pass unchanged.

**1.1** Create `pancetta-ft8/src/protocol.rs`:
```rust
pub enum Protocol { Ft8, Ft4, Ft2 }
pub enum ModulationType { Cpfsk, Gfsk { bt: f64 } }

pub struct ProtocolParams {
    pub protocol: Protocol,
    pub num_tones: usize,            // 8 (FT8/FT2) or 4 (FT4)
    pub num_symbols: usize,          // 79 / 105 / TBD
    pub symbol_period: f64,          // 0.16 / 0.048 / ~0.024
    pub tone_spacing: f64,           // 6.25 / 20.833 / ~41.667
    pub costas_array: &'static [u8],
    pub costas_positions: &'static [usize],
    pub data_symbol_ranges: Vec<Range<usize>>,
    pub cycle_duration: f64,         // 15.0 / 7.5 / 3.8
    pub modulation: ModulationType,
    // derived: samples_per_symbol, tx_duration, window_samples
}
```

**1.2** Refactor `Ft8Decoder` to take `ProtocolParams` instead of hardcoded `COSTAS`, `SAMPLES_PER_SYMBOL`, `SPEC_NFFT`, sync positions `[0,36,72]`, data ranges `(7..36).chain(43..72)`, `NUM_SYMBOLS=79`

**1.3** Refactor `Ft8Modulator` to accept variable-length symbols validated against `protocol_params.num_symbols`, use parameterized tone spacing and samples per symbol

**1.4** Refactor `encode_to_symbols` to use parameterized Costas positions/arrays and appropriate Gray code mapping (3-bit for 8-FSK, 2-bit for 4-FSK)

**1.5** Update `Ft8Config`, coordinator, and `TransmitRequest` to carry a `Protocol` field

---

## Phase 2: FT4 Encode + Modulate

**2.1** Implement `ProtocolParams::ft4()`:
- 4-GFSK, BT=1.0, 105 symbols, 0.048s symbol period, 20.833 Hz spacing
- 4 Costas-like sync arrays `[0,1,3,2]` at positions 0, 25, 51, 77 (verify against WSJT-X `ft4_protocol.f90`)
- 7.5s cycle

**2.2** Implement `binary_to_gray_4fsk` in `ldpc.rs` — 2-bit Gray code (174 bits → 87 data symbols)

**2.3** FT4 GFSK modulator path — BT=1.0 Gaussian pulse shaping (vs FT8's pure CPFSK)

**2.4** Tests: encode CQ as FT4, verify 105 symbols (values 0-3), verify sample count = 60480

---

## Phase 3: FT4 Decode

**3.1** FT4 spectrogram: FFT size 1152 (samples_per_symbol=576, freq_osr=2), step 288

**3.2** FT4 sync search using 4-element `[0,1,3,2]` arrays at parameterized positions

**3.3** New `compute_llr_4fsk` — 2 LLRs per symbol (vs 3 for 8-FSK). Different tone-to-bit partition:
- tones {0,1} → bit0=0, tones {2,3} → bit0=1
- tones {0,2} → bit1=0, tones {1,3} → bit1=1

**3.4** Round-trip tests: encode → modulate → decode → verify. Test at SNR ~-17 dB

---

## Phase 4: FT2 (Experimental, feature-gated)

**4.1** Feature-gate behind `ft2` in `pancetta-ft8/Cargo.toml`

**4.2** `ProtocolParams::ft2()`: 8-GFSK, ~288 samples/symbol, ~41.667 Hz spacing, 3.8s cycle. Document that the sync structure is provisional (two incompatible implementations exist — pick Decodium's, label it)

**4.3** Since FT2 uses 8-GFSK like FT8, the Gray code and LLR paths are identical — only timing/sync differ. Should "just work" with correct protocol params

**4.4** All FT2 tests gated behind `#[cfg(feature = "ft2")]`, clear docs about experimental status

---

## Phase 5: Multi-TX (2-3 messages per cycle)

**5.1** New `MultiTransmitRequest` message type:
```rust
MessageType::MultiTransmitRequest {
    requests: Vec<TransmitRequestItem>,  // each: message, freq_offset, qso_id, protocol
}
```

**5.2** Waveform summation in the coordinator's transmitter task:
- Encode + modulate each message independently at its frequency offset
- Sum sample-by-sample, normalize by signal count, apply 0.95 headroom
- Route single summed buffer to audio output

**5.3** Frequency separation guard — minimum `(num_tones × tone_spacing) + 25 Hz` between any two TX signals (75 Hz for FT8, ~110 Hz for FT4)

**5.4** Safety monitor: summed transmission = one TX period for FCC 6-minute rule

**5.5** Tests: modulate two FT8 messages at 1000 Hz and 1200 Hz offsets, sum, verify no clipping, decode both

---

## Phase 6: Multi-RX QSO Tracking

**6.1** Modify autonomous operator `decide()` to emit up to `max_concurrent_qsos` `Transmit` actions per cycle, each with its own frequency offset and QSO ID

**6.2** `FrequencyAllocator`:
- Tracks in-use frequencies (own QSOs + decoded signals from last window)
- New CQ → pick random clear frequency in configured range
- Reply to CQ → use caller's frequency (standard practice)
- Minimum separation enforced between all own TX signals

**6.3** Coordinator bundles multiple `Transmit` actions into `MultiTransmitRequest`

**6.4** Auto-sequencer already has `max_concurrent_qsos` config and limit checking — no changes needed

---

## Phase 7: TUI Changes

**7.1** Multi-QSO status panel: replace single `QsoStatus` with `Vec<QsoStatus>`, show table of active QSOs (callsign, state, freq, SNR, protocol), display "2/3 active"

**7.2** Protocol indicator in station info and band activity panels (which protocol each decode came from)

---

## Phase 8: Testing Strategy

| Test Type | Coverage |
|-----------|----------|
| Unit | `ProtocolParams` constructors produce correct constants |
| Unit | FT4 2-bit Gray code mapping |
| Unit | FT4 4-FSK LLR computation |
| Unit | Multi-TX waveform normalization |
| Round-trip | FT4 encode → modulate → decode → verify |
| Round-trip | FT2 encode → modulate → decode → verify |
| Round-trip | Multi-TX: 2 messages summed → decode both |
| Integration | Mixed FT8+FT4 audio → decode both protocols |
| Regression | All 186 existing FT8 tests pass at every phase |

---

## Execution Order & Dependencies

| Phase | What | Size | Depends On |
|-------|------|------|------------|
| 1 | Protocol abstraction | L | None |
| 2 | FT4 encode/modulate | M | Phase 1 |
| 3 | FT4 decode | M | Phase 1 |
| 4 | FT2 experimental | S | Phase 1 |
| 5 | Multi-TX | M | Phase 1 |
| 6 | Multi-RX QSO | M | Phase 5 |
| 7 | TUI | S | Phases 5-6 |
| 8 | Testing | Ongoing | All |

Phases 2-5 can run in parallel after Phase 1. Each phase is independently useful.

## Key Risks

1. **FT4 sync structure** — must verify exact positions/values against WSJT-X source (`ft4_protocol.f90`)
2. **FT4 4-FSK LLR** — different soft-decision math from 8-FSK, derive from first principles
3. **FT2 instability** — two incompatible specs exist; feature-gate and document
4. **Multi-TX intermod** — independent phase accumulators (already the case) + amplitude normalization should prevent artifacts

## Key Files

- `pancetta-ft8/src/lib.rs` — top-level constants to parameterize
- `pancetta-ft8/src/decoder.rs` — hardcoded Costas positions, symbol counts, FFT sizes
- `pancetta-ft8/src/modulator.rs` — hardcoded symbol count and tone spacing
- `pancetta-ft8/src/encoder.rs` — symbol mapping (Gray code + Costas insertion)
- `pancetta/src/coordinator.rs` — TX task (~line 1770), single TransmitRequest → multi-TX
- `pancetta-qso/src/autonomous.rs` — decision engine, emit multiple Transmit actions
- `pancetta-tui/src/app.rs` — QsoStatus → Vec<QsoStatus>
