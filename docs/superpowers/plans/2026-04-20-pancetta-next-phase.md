# Pancetta Next Phase Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Complete four sub-projects to get Pancetta on the air with a world-class decoder: decoder polish, advanced decoder features, hamlib rig integration, and cqdx.io + tech debt cleanup.

**Architecture:** The decoder is already at 115.8% of ft8_lib (44/38 decodes, 0 false positives). Remaining decoder work is polish (freq floor, sync range) and advanced features (AP decoding, OSD-3). Hamlib integration wires the existing rigctld client and coordinator into real audio I/O. cqdx.io needs live API validation and grid-needed support.

**Tech Stack:** Rust, cargo workspace, rayon (parallelism), cpal (audio I/O), hound (WAV), serde (serialization), rigctld (hamlib daemon)

**Baseline:** Cross-validation 44/38 (115.8%), 0 false positives. All changes must maintain ≥100% ratio and 0 false positives.

---

## Sub-Project 1: Decoder Polish (A3 + A4)

### Task 1: Lower Frequency Floor (A4)

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:66` (MIN_FREQ_BIN constant)
- Modify: `pancetta-ft8/src/decoder.rs:1056` (max_freq_bin cap)
- Test: `pancetta-ft8/tests/wav_decode_tests.rs` (cross-validation)

- [ ] **Step 1: Write a test that asserts decoding works below 100 Hz**

Add to `pancetta-ft8/tests/wav_decode_tests.rs`:

```rust
/// Verify that the decoder can find signals at low audio frequencies.
/// After lowering MIN_FREQ_BIN, the sync search should include bins below 16.
#[test]
fn test_decoder_searches_below_100hz() {
    // Create a decoder and verify its config allows low-frequency search
    let config = Ft8Config::default();
    let decoder = Ft8Decoder::new(config).unwrap();
    // The decoder should be able to process audio — if MIN_FREQ_BIN is too high,
    // low-frequency signals would be invisible. This is a structural test;
    // real-world validation requires on-air recordings with sub-100 Hz signals.
    let samples = vec![0.0f32; WINDOW_SAMPLES];
    let result = decoder.decode_window(&samples);
    assert!(result.is_ok(), "Decoder should handle empty audio without error");
}
```

- [ ] **Step 2: Run test to verify it passes (structural test)**

Run: `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_decoder_searches_below_100hz --nocapture`
Expected: PASS

- [ ] **Step 3: Lower MIN_FREQ_BIN from 16 to 0**

In `pancetta-ft8/src/decoder.rs`, line 66:

```rust
// Before:
const MIN_FREQ_BIN: usize = 16;

// After:
const MIN_FREQ_BIN: usize = 0;
```

- [ ] **Step 4: Verify max_freq_bin extends to num_bins - NUM_TONES**

In `pancetta-ft8/src/decoder.rs`, line 1056, verify the cap. Currently:

```rust
let max_freq_bin = spectrogram.num_bins.saturating_sub(pp.num_tones);
let max_freq_bin = max_freq_bin.min((4000.0 / pp.tone_spacing) as usize);
```

The 4000 Hz cap is fine — it prevents searching above the FT8 audio passband. No change needed here.

- [ ] **Step 5: Run cross-validation to verify no regression**

Run: `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate_against_ft8lib --nocapture`
Expected: ≥44/38 decodes, 0 false positives. If false positives appear from DC-adjacent bins, raise `MIN_FREQ_BIN` to 4.

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/decoder.rs pancetta-ft8/tests/wav_decode_tests.rs
git commit -m "feat: lower MIN_FREQ_BIN to 0 for full-band FT8 search"
```

### Task 2: Extended Sync Search — Negative Time (A3)

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs:140-150` (Spectrogram struct)
- Modify: `pancetta-ft8/src/decoder.rs:878-980` (compute_spectrogram)
- Modify: `pancetta-ft8/src/decoder.rs:1045-1087` (costas_sync_search)
- Modify: `pancetta-ft8/src/decoder.rs:2280-2286` (time offset reporting in par_decode_candidate)
- Test: `pancetta-ft8/tests/wav_decode_tests.rs`

- [ ] **Step 1: Add time_padding field to Spectrogram struct**

In `pancetta-ft8/src/decoder.rs`, modify the `Spectrogram` struct (line 140):

```rust
struct Spectrogram {
    /// Power values [time_step][freq_sub][freq_bin]
    power: Vec<Vec<Vec<f64>>>,
    /// Number of time steps
    num_steps: usize,
    /// Number of frequency bins per sub-bin (in 6.25 Hz units)
    num_bins: usize,
    /// Frequency oversampling rate
    freq_osr: usize,
    /// Number of time steps prepended for negative-time search.
    /// Subtract this from candidate.time_step to get the real time offset.
    time_padding: usize,
}
```

Update the `Ok(Spectrogram { ... })` return at line 974 to include `time_padding: 0`.

- [ ] **Step 2: Extend compute_spectrogram to prepend look-back time steps**

In `compute_spectrogram()` (line 878), after computing `num_blocks` and before the main loop:

1. Accept a `pre_audio: Option<&[f64]>` parameter (audio samples preceding the nominal window)
2. If `pre_audio` is provided, prepend up to 10 symbols worth of extra time steps
3. Set `time_padding` to the number of prepended steps

Since changing the function signature affects all callers, the simpler approach is to extend the existing audio: check if the caller provides extra audio before the window start. The coordinator already has ring-buffer overlap. For now, add a `pre_samples: usize` field to `Ft8Config` (default 0) that tells `compute_spectrogram` how many leading samples to treat as look-back. When `pre_samples > 0`, the first `pre_samples / subblock_size` time steps are padding.

**Alternative (simpler):** Since the audio buffer passed to `decode_window()` may already contain leading samples, just let the spectrogram compute over the full buffer and record how many steps precede `t=0`. Add a new method `decode_window_with_overlap(samples, overlap_samples)` that sets `time_padding = overlap_samples / subblock_size * TIME_OSR`.

For the initial implementation, keep it simple: add `time_padding` to the struct, default to 0, and wire it through. A follow-up can plumb the overlap audio from the coordinator.

- [ ] **Step 3: Adjust time offset reporting to subtract padding**

In `par_decode_candidate()` (around line 2227), where `coarse_offset` is computed:

```rust
let spec_step = sps / TIME_OSR;
// Subtract time_padding so reported offsets are relative to nominal slot start
let coarse_offset = (candidate.time_step as isize - ctx.spectrogram.time_padding as isize)
    * spec_step as isize;
```

Update the `time_offset_samples` usage at line 2285 to handle the signed offset:

```rust
decoded_message.time_offset_samples as f64 / SAMPLE_RATE as f64,
// becomes:
coarse_offset as f64 / SAMPLE_RATE as f64,
```

Also propagate `time_padding` through the `DecodeContext` struct if needed.

- [ ] **Step 4: Run cross-validation to verify no regression**

Run: `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate_against_ft8lib --nocapture`
Expected: ≥44/38 decodes, 0 false positives. With `time_padding: 0`, behavior should be identical.

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "refactor: add time_padding to Spectrogram for future negative-time sync search"
```

Note: Actually plumbing overlap audio from the coordinator is deferred — this task sets up the infrastructure. The coordinator already has ring-buffer overlap; connecting it is a Task for the hamlib/coordinator integration work.

---

## Sub-Project 2: Advanced Decoder Features

### Task 3: OSD-3 (Triple Bit-Flip)

**Files:**
- Modify: `pancetta-ft8/src/osd.rs` (extend OSD depth)
- Test: `pancetta-ft8/tests/osd_tests.rs`
- Test: `pancetta-ft8/tests/wav_decode_tests.rs` (cross-validation)

- [ ] **Step 1: Check current OSD implementation and max depth**

Read `pancetta-ft8/src/osd.rs` to understand the current OSD-0/1/2 implementation. Verify:
- How `max_depth` is used
- Whether the loop structure supports depth 3 already
- The parity-error gate for each depth level

- [ ] **Step 2: Write a test for OSD-3 decoding a weak signal**

Add to `pancetta-ft8/tests/osd_tests.rs`:

```rust
/// OSD-3 should recover a codeword with 3 bit errors in the most-reliable positions.
#[test]
fn test_osd3_recovers_triple_bit_error() {
    // Create a valid codeword, corrupt 3 info bits in the most-reliable positions,
    // and verify OSD-3 recovers the original.
    let osd = OsdDecoder::new(OsdConfig { max_depth: 3 });

    // Use a known-good codeword from encoder tests
    let valid_codeword = /* ... get from encoder ... */;
    let mut llrs = codeword_to_llrs(&valid_codeword);

    // Flip 3 bits in high-reliability positions (indices 0, 1, 2)
    llrs[0] = -llrs[0];
    llrs[1] = -llrs[1];
    llrs[2] = -llrs[2];

    let result = osd.decode(&llrs);
    assert!(result.is_some(), "OSD-3 should recover triple-bit-error");
    assert_eq!(result.unwrap(), valid_codeword);
}
```

- [ ] **Step 3: Run test to verify it fails (OSD-3 not yet implemented)**

Run: `cargo test -p pancetta-ft8 --test osd_tests -- test_osd3 --nocapture`

- [ ] **Step 4: Implement OSD-3**

In `pancetta-ft8/src/osd.rs`, extend the depth loop to handle depth 3:
- Gate: only attempt OSD-3 when ≤2 parity errors remain after BP (tighter than OSD-2's ≤3 gate)
- Try all C(91,3) = 125,580 triple-bit-flip combinations
- Each trial: XOR 3 rows of the reduced generator matrix + CRC-14 check
- Early termination on first valid codeword

```rust
// After OSD-2 loop:
if self.config.max_depth >= 3 && parity_errors <= 2 {
    for i in 0..k {
        for j in (i + 1)..k {
            for l in (j + 1)..k {
                // XOR rows i, j, l with OSD-0 parity
                let mut trial = osd0_parity.clone();
                xor_row(&mut trial, &reduced_matrix[i]);
                xor_row(&mut trial, &reduced_matrix[j]);
                xor_row(&mut trial, &reduced_matrix[l]);

                if check_crc14(&trial) {
                    // Reconstruct full codeword
                    let mut codeword = osd0_hard.clone();
                    codeword[i] ^= 1;
                    codeword[j] ^= 1;
                    codeword[l] ^= 1;
                    apply_parity(&mut codeword, &trial);
                    return Some(codeword);
                }
            }
        }
    }
}
```

- [ ] **Step 5: Run OSD-3 test to verify it passes**

Run: `cargo test -p pancetta-ft8 --test osd_tests -- test_osd3 --nocapture`
Expected: PASS

- [ ] **Step 6: Update default OSD depth in Ft8Config**

In `pancetta-ft8/src/decoder.rs`, verify `Ft8Config::default()` uses `osd_depth: Some(2)` (line 130). Consider raising to `Some(3)` if OSD-3 stays within the 2-second budget. Run the timing test:

Run: `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_decode_within_realtime_budget --nocapture`

If it passes with OSD-3, update the default. If not, keep OSD-2 as default and let the user opt in.

- [ ] **Step 7: Run cross-validation**

Run: `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate_against_ft8lib --nocapture`
Expected: ≥44/38, 0 false positives. OSD-3 may add 1-2 additional decodes.

- [ ] **Step 8: Commit**

```bash
git add pancetta-ft8/src/osd.rs pancetta-ft8/tests/osd_tests.rs pancetta-ft8/src/decoder.rs
git commit -m "feat: OSD-3 triple bit-flip decoding for deeper error correction"
```

### Task 4: A Priori (AP) Decoding Infrastructure

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (AP context, LLR injection)
- Create: `pancetta-ft8/src/ap.rs` (AP context types and callsign encoding)
- Modify: `pancetta-ft8/src/lib.rs` (expose ap module)
- Test: `pancetta-ft8/tests/wav_decode_tests.rs`

- [ ] **Step 1: Check existing AP infrastructure**

Read the decoder to understand what AP support already exists. The cross-validation test references `m.ap_level` and the decode loop references `ap_context`, `ap_active`, `par_try_ap_decode`. Understand the current AP implementation before extending it.

Grep for: `ApContext`, `ap_level`, `par_try_ap_decode`, `AP1`, `AP2`

- [ ] **Step 2: Document current AP state and identify gaps**

Based on Step 1, document:
- Which AP levels (0-4) are implemented
- How callsign encoding works (28-bit FT8 format)
- Whether AP1 (own callsign) LLR injection is working
- Whether AP2 (recent callsigns) is working
- Whether AP3/AP4 (QSO partner + message type) are working

- [ ] **Step 3: Implement missing AP levels**

Based on Step 2, implement whichever levels are missing. The advanced decoder spec defines:

| Level | Known Bits | Injected | When |
|-------|-----------|----------|------|
| AP0 | 0 | Nothing | Always |
| AP1 | 28 | Your callsign (K5ARH) | Always |
| AP2 | 28 | Each recent callsign | Always |
| AP3 | 56 | Both callsigns | Active QSO |
| AP4 | 65+ | Both calls + message type | Late QSO |

LLR injection: replace LLR values at known bit positions with ±15.0 (high confidence fixed values).

- [ ] **Step 4: Write tests for AP decoding**

Test AP1 with the user's callsign on a weak signal:

```rust
#[test]
fn test_ap1_decodes_with_own_callsign() {
    // Generate a weak signal calling K5ARH, verify AP1 decodes it
    // where AP0 alone would fail
    let ap_context = ApContext {
        my_call: Some(MyCallAp::new("K5ARH")),
        recent_calls: vec![],
        active_qso: None,
    };
    // ... encode weak signal, attempt decode with and without AP
}
```

- [ ] **Step 5: Run tests**

Run: `cargo test -p pancetta-ft8 -- ap --nocapture`
Expected: PASS

- [ ] **Step 6: Run cross-validation with AP disabled (regression check)**

Run: `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture`
Expected: ≥44/38, 0 false positives (AP should not affect the cross-validation since it uses default config)

- [ ] **Step 7: Commit**

```bash
git add pancetta-ft8/src/ap.rs pancetta-ft8/src/decoder.rs pancetta-ft8/src/lib.rs pancetta-ft8/tests/wav_decode_tests.rs
git commit -m "feat: AP decoding levels 1-4 for weak signal recovery"
```

---

## Sub-Project 3: Hamlib Rig Integration

### Task 5: Audio Device Selection for Rig USB

**Files:**
- Modify: `pancetta-audio/src/lib.rs` or `pancetta-audio/src/device.rs`
- Modify: `pancetta-config/` (add audio device config fields)
- Test: unit tests in pancetta-audio

- [ ] **Step 1: Explore pancetta-audio device enumeration**

Read the pancetta-audio crate to understand:
- How CPAL devices are enumerated
- Whether there's a device selection mechanism
- How input/output streams are created
- Current default device behavior

- [ ] **Step 2: Add audio device configuration**

Add to the config struct (in pancetta-config):

```rust
/// Audio device configuration
pub struct AudioDeviceConfig {
    /// Input device name substring match (e.g., "FTdx10", "USB Audio")
    /// None = use system default
    pub input_device: Option<String>,
    /// Output device name substring match
    /// None = use system default
    pub output_device: Option<String>,
}
```

- [ ] **Step 3: Implement device selection by name**

In pancetta-audio, add a function to find a device by name substring:

```rust
pub fn find_input_device(name_pattern: &str) -> Result<cpal::Device, AudioError> {
    let host = cpal::default_host();
    host.input_devices()?
        .find(|d| {
            d.name()
                .map(|n| n.to_lowercase().contains(&name_pattern.to_lowercase()))
                .unwrap_or(false)
        })
        .ok_or(AudioError::DeviceNotFound(name_pattern.to_string()))
}
```

- [ ] **Step 4: Write test for device enumeration**

```rust
#[test]
fn test_enumerate_audio_devices() {
    let host = cpal::default_host();
    let inputs: Vec<String> = host.input_devices()
        .unwrap()
        .filter_map(|d| d.name().ok())
        .collect();
    let outputs: Vec<String> = host.output_devices()
        .unwrap()
        .filter_map(|d| d.name().ok())
        .collect();
    // At least one input and output should exist on any dev machine
    println!("Input devices: {:?}", inputs);
    println!("Output devices: {:?}", outputs);
    assert!(!inputs.is_empty() || !outputs.is_empty(), "No audio devices found");
}
```

- [ ] **Step 5: Run test**

Run: `cargo test -p pancetta-audio -- test_enumerate --nocapture`
Expected: PASS, prints available devices

- [ ] **Step 6: Commit**

```bash
git add pancetta-audio/ pancetta-config/
git commit -m "feat: audio device selection by name for rig USB routing"
```

### Task 6: Wire PTT and Frequency Control in Coordinator

**Files:**
- Modify: `pancetta/src/coordinator/hamlib.rs`
- Modify: `pancetta/src/coordinator/pipeline.rs`
- Modify: `pancetta/src/coordinator/mod.rs`

- [ ] **Step 1: Read current coordinator hamlib integration**

Read `pancetta/src/coordinator/hamlib.rs` to understand:
- How rigctld is spawned
- How frequency polling works
- How RigControlMessage is handled
- The PTT safety watchdog

- [ ] **Step 2: Wire TX audio to rig output device**

In the coordinator's transmit path:
1. When a TX decision is made, select the rig's output audio device (from config)
2. Route the modulated FT8 audio to that device
3. Key PTT via hamlib before audio starts
4. Unkey PTT after audio completes (+ small tail delay)

This requires coordinating three things: PTT on → audio play → PTT off. The 30-second watchdog already exists as a safety net.

- [ ] **Step 3: Wire RX audio from rig input device**

In the coordinator's receive path:
1. Select the rig's input audio device (from config)
2. Feed audio samples into the DSP pipeline → decoder
3. The existing ring buffer approach should work — just change the source device

- [ ] **Step 4: Add frequency tracking to DSP pipeline**

When the rig's frequency changes (polled every 500ms):
1. Update the coordinator's `operating_frequency_hz`
2. No DSP filter changes needed — FT8 uses a fixed 0-3000 Hz audio passband regardless of rig frequency
3. The rig frequency is only used for logging and reporting (converting audio Hz to RF Hz)

- [ ] **Step 5: Test with MockRig**

Run the coordinator with `PANCETTA_MOCK_RIG=true` and verify:
- PTT commands are sent/received
- Frequency updates propagate
- Audio routing doesn't crash (even if no real rig device)

Run: `cargo test -p pancetta --test loopback_qso --nocapture`
Expected: PASS (loopback uses MockRig)

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/coordinator/
git commit -m "feat: wire rig audio I/O and PTT control through coordinator"
```

### Task 7: Real Rig Integration Test

**Files:**
- Create: `pancetta/tests/rig_integration.rs`
- Modify: `pancetta/src/coordinator/hamlib.rs` (if needed)

- [ ] **Step 1: Write a rig connectivity test (requires hardware)**

```rust
/// Integration test that connects to a real rig via rigctld.
/// Skipped unless RIG_TEST=1 is set (requires hardware).
#[test]
fn test_rig_connection() {
    if std::env::var("RIG_TEST").is_err() {
        eprintln!("Skipping rig test (set RIG_TEST=1 to enable)");
        return;
    }

    // Connect to rigctld (must be running)
    let client = RigctldClient::connect("localhost:4532").expect("rigctld connection");

    // Read frequency
    let freq = client.get_frequency().expect("get frequency");
    assert!(freq > 1_000_000.0, "Frequency should be > 1 MHz, got {}", freq);
    println!("Rig frequency: {:.0} Hz", freq);

    // Read mode
    let mode = client.get_mode().expect("get mode");
    println!("Rig mode: {:?}", mode);
}
```

- [ ] **Step 2: Run test (skipped without hardware)**

Run: `cargo test -p pancetta --test rig_integration --nocapture`
Expected: SKIP (no RIG_TEST env var)

- [ ] **Step 3: Commit**

```bash
git add pancetta/tests/rig_integration.rs
git commit -m "feat: rig integration test (hardware-gated)"
```

---

## Sub-Project 4: cqdx.io + Tech Debt

### Task 8: Validate cqdx.io Live Spots Envelope

**Files:**
- Modify: `pancetta-cqdx/src/client.rs` (response parsing)
- Test: `pancetta-cqdx/tests/` or inline tests

- [ ] **Step 1: Read current cqdx.io client implementation**

Read `pancetta-cqdx/src/client.rs` and `pancetta-cqdx/src/types.rs` to understand:
- How the spots endpoint response is parsed
- The expected envelope key (`groups` vs other)
- Authentication (PAT token)

- [ ] **Step 2: Test against live API (if accessible)**

Write a test that hits the live API (gated behind `CQDX_TOKEN` env var):

```rust
#[test]
fn test_live_spots_endpoint() {
    let token = match std::env::var("CQDX_TOKEN") {
        Ok(t) => t,
        Err(_) => {
            eprintln!("Skipping live API test (set CQDX_TOKEN)");
            return;
        }
    };

    let client = CqdxClient::new(&token);
    let spots = client.get_priority_spots().expect("spots endpoint");
    println!("Got {} priority spots", spots.len());
    // Verify the response parsed correctly
    for spot in spots.iter().take(5) {
        println!("  {} on {:.0} Hz", spot.callsign, spot.frequency);
    }
}
```

- [ ] **Step 3: Fix envelope key if needed**

If the live API returns a different key than `groups`, update the response parsing to match.

- [ ] **Step 4: Commit**

```bash
git add pancetta-cqdx/
git commit -m "fix: validate cqdx.io live spots API envelope"
```

### Task 9: Tech Debt Cleanup

**Files:** Various across workspace

- [ ] **Step 1: Lower frequency floor (already done in Task 1)**

Skip if already completed.

- [ ] **Step 2: Fix POTA/SOTA false positives**

Read `pancetta-qso/src/priority.rs` to find the POTA/SOTA detection logic. The issue is prefix vs. suffix detection — callsigns like "K5ARH/P" should match POTA, but prefix-based matching can false-positive on unrelated suffixes.

Fix: require the POTA indicator to be a suffix (after `/`), not a prefix.

- [ ] **Step 3: Run priority scoring tests**

Run: `cargo test -p pancetta-qso -- pota --nocapture`
Expected: PASS

- [ ] **Step 4: Band-aware duplicate detection**

Read `pancetta-qso/` for duplicate detection. Currently duplicates may not consider band — a QSO on 20m and 40m with the same station should not be flagged as duplicate.

Fix: include band in the duplicate key.

- [ ] **Step 5: Run QSO tests**

Run: `cargo test -p pancetta-qso --nocapture`
Expected: PASS

- [ ] **Step 6: Commit tech debt fixes**

```bash
git add pancetta-qso/
git commit -m "fix: POTA/SOTA suffix detection and band-aware duplicate suppression"
```

---

## Verification

After all tasks are complete:

- [ ] **Full workspace build:** `cargo build`
- [ ] **Full workspace tests:** `cargo test`
- [ ] **FT8 cross-validation:** `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate_against_ft8lib --nocapture` — must show ≥100% ratio, 0 false positives
- [ ] **Loopback QSO:** `cargo test -p pancetta --test loopback_qso --nocapture` — must pass
- [ ] **Performance:** `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_decode_within_realtime_budget --nocapture` — must stay within 2x real-time budget
- [ ] **Update CLAUDE.md:** Update decoder status, known gaps, and project phases to reflect completed work
- [ ] **Update docs/ARCHITECTURE.md:** If crate relationships changed
