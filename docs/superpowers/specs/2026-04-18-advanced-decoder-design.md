# Advanced FT8 Decoder — Design Spec

## Goal

Push Pancetta's FT8 decoder beyond the state of the art: A Priori decoding (+4 dB), parallel decode, OSD-3, block detection, and multi-mode signal subtraction. Target: 160%+ of ft8_lib sensitivity at under 2.0s/window, with a decode floor of -28 dB SNR.

## Current State

- Cross-validation: 48/38 (126% of ft8_lib) at 1.3s/window
- Decode floor: approximately -20 dB without AP
- No A Priori decoding, no parallelism, OSD-2, single-mode subtraction
- WSJT-X achieves -24 dB with full AP; JTDX claims -26 dB
- Our target: -28 dB (beyond any published FT8 decoder)

## Sub-project Decomposition

This spec covers three sub-projects that can be implemented sequentially:

1. **AP Decoding** — the +4 dB feature (biggest impact)
2. **Parallel Decode + Budget Management** — enables more compute within time budget
3. **Advanced Decode Techniques** — OSD-3, block detection, multi-mode subtraction

---

## Sub-project 1: A Priori (AP) Decoding

### AP Levels

Five AP levels, run in order. Each level injects progressively more known bits into the LDPC decoder as high-confidence LLRs (±15.0), reducing the effective code rate and improving decode probability.

| Level | Known Bits | What's Injected | When | Sensitivity Gain |
|-------|-----------|-----------------|------|-----------------|
| AP0 | 0 | Nothing (standard decode) | Always | Baseline |
| AP1 | 28 | Your callsign (K1ABC) | Always | +2-3 dB |
| AP2 | 28 | Each recent callsign (excluding already-decoded) | Always | +2-3 dB per call |
| AP3 | 56 | Both callsigns (you + QSO partner) | Active QSO | +3-4 dB |
| AP4 | 65+ | Both calls + expected message type | Late QSO (expecting RR73/73) | +4+ dB |

### AP Context

The coordinator provides an `ApContext` to the decoder each window:

```rust
pub struct ApContext {
    /// Our callsign, always available
    pub my_call: String,
    /// Our callsign pre-encoded as 28-bit integer
    pub my_call_28: u32,
    /// Recently seen callsigns (last 2-3 windows), strongest first
    /// Excluding any already decoded in the current window
    pub recent_calls: Vec<RecentCall>,
    /// Active QSO information, if any
    pub active_qso: Option<QsoApInfo>,
}

pub struct RecentCall {
    pub callsign: String,
    pub call_28: u32,
    pub last_snr: f32,
}

pub struct QsoApInfo {
    pub their_call: String,
    pub their_call_28: u32,
    /// QSO progress determines which AP levels to try
    pub progress: QsoProgress,
}

pub enum QsoProgress {
    /// Sent CQ or call, expecting grid/report
    WaitingForReport,
    /// Received report, expecting RRR/RR73/73
    WaitingForConfirmation,
    /// Received confirmation
    Complete,
}
```

### AP Decode Flow

```
decode_window(audio, ap_context):
    spectrogram = compute_spectrogram(audio)
    candidates = costas_sync_search(spectrogram)
    decoded = []

    // Pass 0: AP0 + AP1 on all candidates
    for candidate in candidates (parallel):
        result = try_decode(candidate, spectrogram, AP0)
        if result.is_none():
            result = try_decode(candidate, spectrogram, AP1, my_call_28)
        if result.is_some():
            decoded.push(result)

    // Subtract decoded signals
    subtract_all(audio, decoded)

    // Collect already-decoded callsigns for exclusion
    decoded_calls = set(decoded.map(|d| d.callsigns()))

    // Pass 1: AP2 on remaining candidates with recent callsigns
    spectrogram = compute_spectrogram(audio)  // recompute after subtraction
    candidates = costas_sync_search(spectrogram)
    ap2_calls = recent_calls.filter(|c| !decoded_calls.contains(c))
                            .take(20)

    for candidate in candidates (parallel):
        result = try_decode(candidate, spectrogram, AP0)
        if result.is_none():
            result = try_decode(candidate, spectrogram, AP1, my_call_28)
        if result.is_none():
            for call in ap2_calls:
                if decoded_calls.contains(call): continue  // short-circuit
                result = try_decode(candidate, spectrogram, AP2, call.call_28)
                if result.is_some():
                    decoded_calls.insert(call)
                    break
        if result.is_some():
            decoded.push(result)

    // Pass 2: AP3/AP4 during active QSO
    if active_qso.is_some():
        subtract_all(audio, new_decoded)
        spectrogram = compute_spectrogram(audio)
        candidates = costas_sync_search(spectrogram)
        for candidate in candidates (parallel):
            // Try AP3 (both callsigns known)
            result = try_decode(candidate, spectrogram, AP3,
                               my_call_28, their_call_28)
            if result.is_none() && progress == WaitingForConfirmation:
                // Try AP4 (both calls + expected message type)
                result = try_decode(candidate, spectrogram, AP4,
                                   my_call_28, their_call_28, expected_type)
            if result.is_some():
                decoded.push(result)

    return decoded
```

### LLR Injection

AP bits are injected by replacing LLR values at known bit positions before running LDPC:

```rust
fn inject_ap_bits(llrs: &mut [f32], known_bits: &[(usize, bool)]) {
    const AP_CONFIDENCE: f32 = 15.0;
    for &(bit_pos, bit_val) in known_bits {
        // Convention: negative LLR = bit is 1
        llrs[bit_pos] = if bit_val { -AP_CONFIDENCE } else { AP_CONFIDENCE };
    }
}
```

The bit positions for callsigns in the FT8 77-bit payload:
- Callsign 1 (calling station): bits 0-27 (28 bits)
- Callsign 2 (called station): bits 28-55 (28 bits)  
- Report/grid/message type: bits 56-76 (21 bits)

For AP1 (your callsign as called station): inject bits 28-55.
For AP2 (recent callsign as calling station): inject bits 0-27.
For AP3 (both): inject bits 0-55.
For AP4 (both + type): inject bits 0-55 + partial 56-76.

### Callsign Encoding

FT8 encodes callsigns as 28-bit integers using the formula from the FT8 spec:
```
N = 36*A + B  (for each character position)
where A-Z = 0-25, 0-9 = 26-35, space = 36
Result = c1*36^5 + c2*36^4 + c3*36^3 + c4*36^2 + c5*36 + c6
```

This function already exists in the encoder module (`pancetta-ft8/src/message.rs` or similar). Expose it as a public utility.

### Recent Callsign Pool Management

The `ApContext.recent_calls` pool:
- Maintained by the coordinator, not the decoder
- Populated from decoded messages in the last 2-3 windows
- Sorted by last SNR (strongest first — more likely to appear again)
- Capped at 20 entries
- Excluded: any callsign already decoded in the current window (short-circuit)
- Updated after each window's decodes complete

---

## Sub-project 2: Parallel Decode + Budget Management

### Parallelism via Rayon

Add `rayon` dependency. Parallelize at the candidate level using `par_iter`:

```rust
let results: Vec<Option<DecodedMessage>> = candidates
    .par_iter()
    .map(|candidate| {
        self.decode_candidate(candidate, &spectrogram, &ap_context)
    })
    .collect();
```

The spectrogram is read-only during candidate decoding — no synchronization needed. Each candidate's decode is independent (own LDPC state, own OSD state).

### DecodeConfig

```rust
pub struct DecodeConfig {
    pub max_candidates_pass0: usize,    // default: 100
    pub max_candidates_passN: usize,    // default: 40
    pub max_decode_passes: usize,       // default: 3
    pub parallelism: Parallelism,       // default: Rayon
    pub budget_ms: u64,                 // default: 2000
    pub ap_config: ApConfig,
}

pub enum Parallelism {
    Serial,
    Rayon { max_threads: Option<usize> },
}

pub struct ApConfig {
    pub enabled: bool,                  // default: true
    pub max_recent_calls: usize,        // default: 20
    pub ap3_enabled: bool,              // default: true (during QSO)
    pub ap4_enabled: bool,              // default: true (late QSO)
}
```

### Budget Enforcement

The decode loop checks elapsed time after each pass. If `budget_ms` is exceeded, it returns what it has:

```rust
let deadline = Instant::now() + Duration::from_millis(config.budget_ms);

for pass in 0..config.max_decode_passes {
    if Instant::now() >= deadline {
        info!("Decode budget exceeded after pass {}", pass);
        break;
    }
    // ... decode pass ...
}
```

Individual candidate decodes also check the deadline before starting expensive operations (DFT fallback, OSD).

### Thread Safety

`Ft8Decoder` currently uses `&mut self` for `decode_window` (buffer reuse). For parallel candidate decoding, the candidate-level work must not require `&mut self`. Options:
- Move per-candidate buffers (FFT buffers, LLR arrays) to thread-local storage
- Allocate per-candidate buffers inside the parallel closure
- Use `Ft8Decoder` only for spectrogram computation (serial), then pass immutable state to parallel candidate decoders

The recommended approach: extract a `CandidateDecoder` struct that holds per-thread LDPC and OSD state, created per rayon thread via `thread_local!` or cloned cheaply.

---

## Sub-project 3: Advanced Decode Techniques

### OSD-3

Raise default `osd_depth` from 2 to 3. OSD-3 tries `C(91,3) = 125,580` triple-bit-flip combinations. Gate on ≤2 remaining parity errors (tighter than OSD-2's ≤3) to limit false positives and CPU cost.

Cost: ~125K CRC-14 checks per candidate that triggers OSD-3. At ~10ns per CRC check = ~1.3ms per candidate. With rayon parallelism across 8 cores, this is negligible.

Retain message validation (`is_plausible()`) to filter CRC false positives from the larger trial count.

### Block Detection

After standard per-symbol extraction, compute a coherent block score:
1. For each candidate, sum the spectrogram power at all 21 Costas sync positions (known tones) and all 58 data positions (best tone per symbol)
2. Compare against the noise floor (average power at non-signal bins)
3. The block score provides a better candidate ranking than the Costas-only sync score

Use block scores to re-rank candidates before LDPC — best candidates get decoded first, improving success rate within the time budget.

Implementation:
```rust
fn block_score(spec: &Spectrogram, candidate: &CostasCandidate,
               symbols: &[u8]) -> f64 {
    let mut signal_power = 0.0;
    let mut noise_power = 0.0;
    let mut signal_count = 0;
    let mut noise_count = 0;

    for sym_idx in 0..NUM_SYMBOLS {
        let t = candidate.time_step + sym_idx * steps_per_symbol;
        let expected_tone = symbols[sym_idx] as usize;

        for tone in 0..NUM_TONES {
            let power = spec.power[t][candidate.freq_sub][candidate.freq_bin + tone];
            if tone == expected_tone {
                signal_power += power;
                signal_count += 1;
            } else {
                noise_power += power;
                noise_count += 1;
            }
        }
    }

    let avg_signal = signal_power / signal_count as f64;
    let avg_noise = noise_power / noise_count as f64;
    avg_signal - avg_noise  // dB difference
}
```

### Multi-Mode Signal Subtraction

Current subtraction removes the signal at its center frequency. Add sidelobe cancellation:

After subtracting the main signal at frequency `f0`:
1. Also subtract scaled copies at `f0 ± 6.25 Hz` (one tone spacing) with amplitude estimated from the spectral sidelobe pattern
2. The sidelobe amplitude is approximately `0.15×` the main signal for a Hann-windowed FFT

```rust
fn subtract_with_sidelobes(audio: &mut [f32], msg: &DecodedMessage) {
    // Main signal subtraction (existing)
    subtract_signal(audio, msg, msg.frequency_offset, 1.0);

    // Sidelobe cancellation at ±1 tone spacing
    let sidelobe_factor = 0.15;
    subtract_signal(audio, msg, msg.frequency_offset + TONE_SPACING, sidelobe_factor);
    subtract_signal(audio, msg, msg.frequency_offset - TONE_SPACING, sidelobe_factor);
}
```

This 3× subtraction cleans up spectral leakage that masks weak signals at adjacent frequencies, significantly improving pass 2+ decode rates on dense bands.

---

## Success Criteria

| Metric | Current | Target |
|--------|---------|--------|
| Cross-validation ratio | 126% of ft8_lib | 160%+ |
| Decode floor (no AP) | ~-20 dB | -22 dB |
| Decode floor (AP1, own call) | N/A | -24 dB |
| Decode floor (AP3, both calls) | N/A | -26 dB |
| Decode floor (AP4, full AP) | N/A | -28 dB (stretch) |
| Speed (release, 8-core) | 1.3s/window | <2.0s/window |
| Speed (serial, debug) | 4.7s/window | <5.0s/window |
| Regression floor | 95% | 120% |

### New Tests

- `test_ap1_decode_at_minus_22db`: Encode "CQ K1ABC FN42" at -22 dB, verify AP1 decodes it
- `test_ap3_decode_at_minus_26db`: Encode "W1ABC K1ABC -18" at -26 dB with both calls known, verify AP3 decodes
- `test_parallel_matches_serial`: Same input, same results regardless of parallelism mode
- `test_budget_cutoff`: Force 100ms budget, verify partial results returned without panic
- `test_sidelobe_subtraction`: Encode two signals 50 Hz apart, verify both decoded in multi-pass

---

## File Map

| File | Changes |
|------|---------|
| `pancetta-ft8/src/decoder.rs` | AP injection, parallel dispatch, block scoring, decode config |
| `pancetta-ft8/src/ap.rs` | New: ApContext, callsign encoding, LLR injection, recent call pool |
| `pancetta-ft8/src/parallel.rs` | New: CandidateDecoder, rayon integration, budget management |
| `pancetta-ft8/src/osd.rs` | OSD-3 support (already handles arbitrary depth) |
| `pancetta-ft8/src/lib.rs` | New config fields, public API for ApContext |
| `pancetta-ft8/Cargo.toml` | Add rayon dependency |
| `pancetta/src/coordinator/pipeline.rs` | Build ApContext from QSO state + recent decodes |
| `pancetta-ft8/tests/wav_decode_tests.rs` | AP sensitivity tests, parallel determinism test |

---

## Future Work: Neural Network OSD Optimization

The paper "Boosting Ordered Statistics Decoding of Short LDPC Codes with Simple Neural Network Models" (Li et al., 2024) demonstrates that a lightweight CNN can predict which bit-flip patterns are most likely to succeed, reducing OSD trial count from thousands to ~175 while maintaining near-ML performance.

**Key findings from the paper:**
- A 4-layer Decoding Information Aggregation (DIA) model processes the trajectory of LLRs across LDPC iterations to refine bit reliability estimates
- A Sliding Window-Assisted (SWA) model enables early termination of OSD, reducing average patterns tested from ~2,500 to ~175
- At FER=10⁻⁴ for LDPC(128,64): achieves within 0.06 dB of maximum likelihood performance
- DIA requires 167.7K FLOPs; SWA requires 0.06K FLOPs — negligible overhead
- Tested on LDPC CCSDS(128,64); transferable to FT8's LDPC(174,91) with retraining

**Applicability to Pancetta:** Train DIA+SWA models on FT8's specific LDPC(174,91) code using simulated AWGN channels at target SNRs (-20 to -28 dB). Deploy as a small inference model (ONNX or pure Rust) that gates OSD trials. This would enable OSD-4 or higher without the exponential trial count, potentially pushing the decode floor to -30 dB.

**References:**
- [Boosting OSD of Short LDPC Codes with Neural Networks](https://arxiv.org/abs/2404.14165) — Li et al., IEEE Communications Letters, 2024
- [The FT4 and FT8 Communication Protocols](https://wsjt.sourceforge.io/FT4_FT8_QEX.pdf) — Taylor, Franke, Somerville (QEX paper)
- [weakmon FT8 decoder](https://github.com/rtmrtmrtmrtm/weakmon/blob/master/ft8.py) — reference Python implementation with OSD-6 and AP
- [Boosted Neural Decoders for 6G LDPC](https://arxiv.org/abs/2405.13413) — related neural LDPC work

## Non-Goals

- GPU acceleration (future, not needed at current scale)
- FT4 protocol support (separate project)
- Changing the FT8 modulation or coding scheme (protocol-defined)
- Real-time streaming decode (we decode complete 15s windows)
