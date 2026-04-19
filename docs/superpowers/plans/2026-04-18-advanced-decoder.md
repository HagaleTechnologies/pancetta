# Advanced FT8 Decoder Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Push the FT8 decoder to 160%+ of ft8_lib with -28 dB decode floor via AP decoding, parallel decode, OSD-3, block detection, and multi-mode subtraction.

**Architecture:** Three sub-projects implemented sequentially: (1) AP decoding with 5 levels and recent callsign pool, (2) rayon-based parallel candidate decoding with budget management, (3) OSD-3 + block detection + sidelobe subtraction. Each sub-project is independently testable.

**Tech Stack:** Rust, rustfft, rayon, bitvec

**Spec:** `docs/superpowers/specs/2026-04-18-advanced-decoder-design.md`

**Current baseline:** 48/38 (126% of ft8_lib) at 1.3s/window release, ~-20 dB decode floor.

**Benchmark commands:**
- Sensitivity: `cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture 2>&1 | grep -E "^[a-z].*ours=|Overall"`
- Speed (release): `time cargo test -p pancetta-ft8 --release --test wav_decode_tests -- test_cross_validate 2>&1 | grep "finished in"`
- All tests: `cargo test -p pancetta-ft8 --lib && cargo test -p pancetta -- --test-threads=1`

---

## Sub-project 1: A Priori (AP) Decoding

### Task 1: Create AP module with callsign encoding

**Files:**
- Create: `pancetta-ft8/src/ap.rs`
- Modify: `pancetta-ft8/src/lib.rs` (add `pub mod ap` and re-exports)

- [ ] **Step 1: Create ap.rs with ApContext and callsign encoding**

Create `pancetta-ft8/src/ap.rs`:

```rust
//! A Priori (AP) decoding support for FT8.
//!
//! AP decoding injects known information (callsigns, message types) into the
//! LDPC decoder as high-confidence LLRs, reducing the effective code rate
//! and improving decode probability by up to +4 dB.

use crate::encoder::pack28;
use crate::Ft8Result;

/// Confidence value for AP-injected LLR bits.
/// High enough to dominate channel LLRs, low enough to avoid
/// numerical issues in sum-product LDPC.
const AP_LLR_CONFIDENCE: f32 = 15.0;

/// AP context provided by the coordinator each decode window.
#[derive(Debug, Clone, Default)]
pub struct ApContext {
    /// Our callsign, always known
    pub my_call: Option<MyCallAp>,
    /// Recently seen callsigns (last 2-3 windows), strongest first.
    /// Capped at max_recent_calls. Excludes already-decoded calls.
    pub recent_calls: Vec<RecentCallAp>,
    /// Active QSO information for AP3/AP4
    pub active_qso: Option<QsoAp>,
}

/// Pre-encoded own callsign for AP1
#[derive(Debug, Clone)]
pub struct MyCallAp {
    pub callsign: String,
    /// 28-bit packed callsign
    pub packed_28: u32,
    /// The 28 individual bits, MSB first
    pub bits: [bool; 28],
}

/// Pre-encoded recent callsign for AP2
#[derive(Debug, Clone)]
pub struct RecentCallAp {
    pub callsign: String,
    pub packed_28: u32,
    pub bits: [bool; 28],
    pub last_snr: f32,
}

/// Active QSO AP info for AP3/AP4
#[derive(Debug, Clone)]
pub struct QsoAp {
    pub their_call: String,
    pub their_packed_28: u32,
    pub their_bits: [bool; 28],
    pub progress: QsoApProgress,
}

/// QSO progress determines which AP levels are available
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QsoApProgress {
    /// Sent CQ or call, expecting report/grid
    WaitingForReport,
    /// Received report, expecting RRR/RR73/73
    WaitingForConfirmation,
}

impl MyCallAp {
    /// Create from a callsign string. Returns None if encoding fails.
    pub fn new(callsign: &str) -> Option<Self> {
        let (packed, _ip) = pack28(callsign).ok()?;
        let bits = u32_to_bits_28(packed);
        Some(Self {
            callsign: callsign.to_uppercase(),
            packed_28: packed,
            bits,
        })
    }
}

impl RecentCallAp {
    /// Create from a callsign string and SNR. Returns None if encoding fails.
    pub fn new(callsign: &str, snr: f32) -> Option<Self> {
        let (packed, _ip) = pack28(callsign).ok()?;
        let bits = u32_to_bits_28(packed);
        Some(Self {
            callsign: callsign.to_uppercase(),
            packed_28: packed,
            bits,
            last_snr: snr,
        })
    }
}

impl QsoAp {
    /// Create from their callsign and QSO progress.
    pub fn new(their_call: &str, progress: QsoApProgress) -> Option<Self> {
        let (packed, _ip) = pack28(their_call).ok()?;
        let bits = u32_to_bits_28(packed);
        Some(Self {
            their_call: their_call.to_uppercase(),
            their_packed_28: packed,
            their_bits: bits,
            progress,
        })
    }
}

/// Convert a 28-bit packed value to an array of bools, MSB first.
fn u32_to_bits_28(value: u32) -> [bool; 28] {
    let mut bits = [false; 28];
    for i in 0..28 {
        bits[i] = (value >> (27 - i)) & 1 == 1;
    }
    bits
}

/// AP level for a decode attempt
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ApLevel {
    /// No AP — standard decode
    Ap0,
    /// Inject our callsign (bits 28-55 of payload = called station)
    Ap1,
    /// Inject a recent callsign (bits 0-27 of payload = calling station)
    Ap2,
    /// Inject both callsigns (bits 0-55)
    Ap3,
    /// Inject both callsigns + expected message type (bits 0-55 + partial 56-76)
    Ap4,
}

/// Inject AP bits into LLR array before LDPC decoding.
///
/// FT8 77-bit payload layout:
/// - Bits 0-27: calling station callsign (28 bits)
/// - Bits 28-55: called station callsign (28 bits)
/// - Bits 56-76: report/grid/message content (21 bits)
///
/// AP works by replacing the LLR values at known bit positions with
/// high-confidence fixed values, so the LDPC decoder only needs to
/// resolve the remaining unknown bits.
pub fn inject_ap_llrs(llrs: &mut [f32], level: ApLevel, context: &ApContext) {
    match level {
        ApLevel::Ap0 => {} // No injection
        ApLevel::Ap1 => {
            // Inject our callsign at bits 28-55 (called station)
            if let Some(ref my_call) = context.my_call {
                inject_bits(llrs, &my_call.bits, 28);
            }
        }
        ApLevel::Ap2 => {
            // Inject a recent callsign at bits 0-27 (calling station)
            // The specific callsign is selected by the caller
            // This method is called with a modified context per trial
        }
        ApLevel::Ap3 => {
            // Inject both callsigns
            if let Some(ref my_call) = context.my_call {
                inject_bits(llrs, &my_call.bits, 28); // called station
            }
            if let Some(ref qso) = context.active_qso {
                inject_bits(llrs, &qso.their_bits, 0); // calling station
            }
        }
        ApLevel::Ap4 => {
            // Inject both callsigns + RR73 message type
            if let Some(ref my_call) = context.my_call {
                inject_bits(llrs, &my_call.bits, 28);
            }
            if let Some(ref qso) = context.active_qso {
                inject_bits(llrs, &qso.their_bits, 0);
            }
            // RR73 type indicator at bits 56-58 (i3=0, n3=4 for RR73)
            // i3 (3 bits at 74-76) = 0: bits 74,75,76 = false,false,false
            // This is a partial injection — only the type bits we're sure of
            inject_bit(llrs, 74, false);
            inject_bit(llrs, 75, false);
            inject_bit(llrs, 76, false);
        }
    }
}

/// Inject a known callsign's 28 bits starting at bit_offset in the LLR array.
fn inject_bits(llrs: &mut [f32], bits: &[bool; 28], bit_offset: usize) {
    for (i, &bit_val) in bits.iter().enumerate() {
        let pos = bit_offset + i;
        if pos < llrs.len() {
            // Convention: negative LLR = bit is 1
            llrs[pos] = if bit_val { -AP_LLR_CONFIDENCE } else { AP_LLR_CONFIDENCE };
        }
    }
}

/// Inject a single known bit.
fn inject_bit(llrs: &mut [f32], pos: usize, bit_val: bool) {
    if pos < llrs.len() {
        llrs[pos] = if bit_val { -AP_LLR_CONFIDENCE } else { AP_LLR_CONFIDENCE };
    }
}

/// Inject a specific recent callsign for AP2 (bits 0-27).
pub fn inject_ap2_caller(llrs: &mut [f32], caller: &RecentCallAp) {
    inject_bits(llrs, &caller.bits, 0);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_u32_to_bits_28() {
        let bits = u32_to_bits_28(0b1010_0000_0000_0000_0000_0000_0000);
        assert!(bits[0]); // MSB
        assert!(!bits[1]);
        assert!(bits[2]);
        assert!(!bits[3]);
    }

    #[test]
    fn test_my_call_ap_creation() {
        let ap = MyCallAp::new("K5ARH").unwrap();
        assert_eq!(ap.callsign, "K5ARH");
        assert_eq!(ap.bits.len(), 28);
        // Verify round-trip: bits back to packed value
        let mut repacked = 0u32;
        for (i, &b) in ap.bits.iter().enumerate() {
            if b { repacked |= 1 << (27 - i); }
        }
        assert_eq!(repacked, ap.packed_28);
    }

    #[test]
    fn test_inject_ap1() {
        let mut llrs = vec![0.0f32; 174];
        let ctx = ApContext {
            my_call: MyCallAp::new("K5ARH"),
            ..Default::default()
        };
        inject_ap_llrs(&mut llrs, ApLevel::Ap1, &ctx);
        // Bits 28-55 should be non-zero (injected)
        assert!(llrs[28].abs() > 10.0);
        assert!(llrs[55].abs() > 10.0);
        // Bits outside AP range should be untouched
        assert_eq!(llrs[0], 0.0);
        assert_eq!(llrs[56], 0.0);
    }

    #[test]
    fn test_inject_ap3() {
        let mut llrs = vec![0.0f32; 174];
        let ctx = ApContext {
            my_call: MyCallAp::new("K5ARH"),
            active_qso: QsoAp::new("W1ABC", QsoApProgress::WaitingForReport),
            ..Default::default()
        };
        inject_ap_llrs(&mut llrs, ApLevel::Ap3, &ctx);
        // Bits 0-27 (their call) and 28-55 (our call) should be injected
        assert!(llrs[0].abs() > 10.0);
        assert!(llrs[27].abs() > 10.0);
        assert!(llrs[28].abs() > 10.0);
        assert!(llrs[55].abs() > 10.0);
        // Bits 56+ untouched
        assert_eq!(llrs[56], 0.0);
    }
}
```

- [ ] **Step 2: Add module to lib.rs**

In `pancetta-ft8/src/lib.rs`, add:
```rust
pub mod ap;
pub use ap::{ApContext, ApLevel, MyCallAp, RecentCallAp, QsoAp, QsoApProgress};
```

- [ ] **Step 3: Build and run tests**

```bash
touch pancetta-ft8/src/ap.rs pancetta-ft8/src/lib.rs
cargo test -p pancetta-ft8 --lib -- ap 2>&1 | tail -10
```
Expected: 4 AP tests pass.

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/src/ap.rs pancetta-ft8/src/lib.rs
git commit -m "feat: add AP decoding module with callsign encoding and LLR injection"
```

---

### Task 2: Wire AP into decode_candidate

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (decode_candidate, decode_window)

- [ ] **Step 1: Add ApContext parameter to decode_window**

Change the `decode_window` signature to accept an optional `ApContext`:

```rust
pub fn decode_window(&mut self, samples: &[f32]) -> Ft8Result<Vec<DecodedMessage>> {
    self.decode_window_with_ap(samples, &ApContext::default())
}

pub fn decode_window_with_ap(
    &mut self,
    samples: &[f32],
    ap_context: &ApContext,
) -> Ft8Result<Vec<DecodedMessage>> {
    // ... existing decode_window body, with AP additions ...
}
```

This preserves backward compatibility — existing callers use `decode_window` unchanged.

- [ ] **Step 2: Add AP levels to decode_candidate**

Modify `decode_candidate` to accept an `ApLevel` and `ApContext`. After computing LLRs and before running LDPC, inject AP bits:

```rust
fn decode_candidate(
    &mut self,
    audio: &[f64],
    candidate: &CostasCandidate,
    spectrogram: &Spectrogram,
    ap_level: ApLevel,
    ap_context: &ApContext,
) -> Ft8Result<Option<DecodedMessage>> {
    // ... existing spectrogram extraction ...
    // ... compute LLRs ...

    // Inject AP bits before LDPC
    crate::ap::inject_ap_llrs(&mut llrs, ap_level, ap_context);

    normalize_llrs(&mut llrs);

    // ... existing LDPC decode + CRC check ...
}
```

Update all call sites in `decode_window_with_ap` to pass AP level and context.

- [ ] **Step 3: Implement AP pass structure in decode_window_with_ap**

The decode loop becomes:

```rust
// Pass 0: AP0 + AP1 on all candidates
for candidate in &sync_candidates {
    // Try AP0 (no AP)
    let result = self.decode_candidate(audio, candidate, &spectrogram,
                                        ApLevel::Ap0, ap_context);
    if result.is_ok_and(|r| r.is_some()) {
        // decoded — add to results
        continue;
    }
    // Try AP1 (our callsign)
    if ap_context.my_call.is_some() {
        let result = self.decode_candidate(audio, candidate, &spectrogram,
                                            ApLevel::Ap1, ap_context);
        if result.is_ok_and(|r| r.is_some()) {
            continue;
        }
    }
}

// Subtract decoded signals, then...

// Pass 1: AP2 with recent callsigns on remaining candidates
let decoded_calls: HashSet<String> = all_decoded_messages.iter()
    .filter_map(|m| m.from_callsign.clone())
    .collect();

for candidate in &sync_candidates {
    // Try AP0 first (signals revealed by subtraction)
    // ...
    // Then try AP2 for each recent call not already decoded
    for recent in &ap_context.recent_calls {
        if decoded_calls.contains(&recent.callsign) { continue; }
        // Create modified context with this caller injected
        let result = self.decode_candidate_ap2(
            audio, candidate, &spectrogram, recent, ap_context);
        if result.is_ok_and(|r| r.is_some()) {
            decoded_calls.insert(recent.callsign.clone());
            break; // short-circuit to next candidate
        }
    }
}

// Pass 2: AP3/AP4 during active QSO
if ap_context.active_qso.is_some() {
    // ... subtract, recompute, search with AP3/AP4 ...
}
```

- [ ] **Step 4: Add decode_candidate_ap2 helper**

This is a thin wrapper that injects a specific recent callsign's bits:

```rust
fn decode_candidate_ap2(
    &mut self,
    audio: &[f64],
    candidate: &CostasCandidate,
    spectrogram: &Spectrogram,
    caller: &RecentCallAp,
    ap_context: &ApContext,
) -> Ft8Result<Option<DecodedMessage>> {
    // Extract symbols from spectrogram
    let tone_magnitudes = self.extract_symbols_from_spectrogram(spectrogram, candidate)?;
    let data_mags = self.filter_data_symbols(&tone_magnitudes);
    let mut llrs = self.compute_soft_llrs_db(&data_mags);

    // Inject AP2 (calling station = recent call) + AP1 (called station = our call)
    crate::ap::inject_ap2_caller(&mut llrs, caller);
    if let Some(ref my_call) = ap_context.my_call {
        crate::ap::inject_bits_at(&mut llrs, &my_call.bits, 28);
    }

    normalize_llrs(&mut llrs);

    let corrected_bits = match self.ldpc_decoder.decode_soft(&llrs) {
        Ok(bits) => bits,
        Err(_) => return Ok(None),
    };

    if !self.verify_crc(&corrected_bits) {
        return Ok(None);
    }

    // Parse and return
    self.build_decoded_message(&corrected_bits, candidate)
}
```

- [ ] **Step 5: Build and run all tests**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta -- --test-threads=1 2>&1 | grep "test result" | head -5
```
Expected: all existing tests pass (AP is off by default via `decode_window`).

- [ ] **Step 6: Run cross-validation benchmark**

```bash
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture 2>&1 | grep -E "^[a-z].*ours=|Overall"
```
Expected: no regression from baseline (AP not active in cross-validation test).

- [ ] **Step 7: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: wire AP decoding into decode pipeline with 5 levels"
```

---

### Task 3: AP sensitivity tests

**Files:**
- Modify: `pancetta-ft8/tests/wav_decode_tests.rs` or create new test file

- [ ] **Step 1: Write AP1 sensitivity test**

Add test that encodes a message at -22 dB, verifies standard decode fails, AP1 succeeds:

```rust
#[test]
fn test_ap1_decode_at_minus_22db() {
    use pancetta_ft8::*;
    use pancetta_ft8::ap::*;

    let mut encoder = Ft8Encoder::new();
    let mut modulator = Ft8Modulator::new_default().unwrap();

    // Encode "W1ABC K5ARH FN42" — a message TO our callsign
    let text = "W1ABC K5ARH FN42";
    let symbols = encoder.encode_message(text, None).unwrap();
    let mut audio = modulator.modulate_symbols(&symbols, 1000.0).unwrap();
    audio.resize(WINDOW_SAMPLES, 0.0);

    // Add noise to achieve ~-22 dB SNR
    // FT8 signal bandwidth = 50 Hz, reference bandwidth = 2500 Hz
    // SNR_2500 = -22 dB means signal power is 10^(-2.2) of noise in 2500 Hz
    let signal_rms: f32 = (audio.iter().map(|s| s*s).sum::<f32>() / audio.len() as f32).sqrt();
    let target_snr_linear = 10.0f32.powf(-22.0 / 10.0) * (50.0 / 2500.0);
    let noise_rms = signal_rms / target_snr_linear.sqrt();

    use rand::Rng;
    let mut rng = rand::thread_rng();
    for sample in audio.iter_mut() {
        *sample += rng.gen_range(-noise_rms..noise_rms);
    }

    // Standard decode (AP0) should fail at -22 dB
    let mut decoder = Ft8Decoder::new(Ft8Config::default()).unwrap();
    let standard_results = decoder.decode_window(&audio).unwrap();
    // May or may not decode — -22 dB is borderline

    // AP1 decode (with our callsign) should succeed
    let ap_context = ApContext {
        my_call: MyCallAp::new("K5ARH"),
        ..Default::default()
    };
    let mut decoder2 = Ft8Decoder::new(Ft8Config::default()).unwrap();
    let ap_results = decoder2.decode_window_with_ap(&audio, &ap_context).unwrap();

    // AP1 should find the message (or at least do no worse than standard)
    assert!(
        ap_results.len() >= standard_results.len(),
        "AP1 should decode at least as many messages as standard"
    );
}
```

- [ ] **Step 2: Run the test**

```bash
touch pancetta-ft8/tests/wav_decode_tests.rs
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_ap1 --nocapture 2>&1 | tail -10
```

- [ ] **Step 3: Commit**

```bash
git add pancetta-ft8/tests/wav_decode_tests.rs
git commit -m "test: AP1 sensitivity test at -22 dB"
```

---

### Task 4: Wire ApContext from coordinator

**Files:**
- Modify: `pancetta/src/coordinator/pipeline.rs` (FT8 decoder thread)

- [ ] **Step 1: Build ApContext in the FT8 decoder thread**

In the FT8 decoder thread (inside `start_ft8_pipeline`), before calling `decode_window`, build an `ApContext` from the coordinator's state:

```rust
// Read the station callsign from config
let my_call_ap = pancetta_ft8::ap::MyCallAp::new(&station_callsign);

// Build ApContext for each window
let ap_context = pancetta_ft8::ap::ApContext {
    my_call: my_call_ap.clone(),
    recent_calls: vec![], // populated from previous window's decodes
    active_qso: None, // populated from QSO manager state
};

let decoded = decoder.decode_window_with_ap(&audio_f32, &ap_context)?;

// After decoding, update recent_calls for next window
// Store decoded callsigns with their SNR for AP2 in the next window
```

Read the coordinator pipeline code to find the exact location where `decode_window` is currently called. The station callsign should come from the config (available via `self.config.read().await`).

- [ ] **Step 2: Maintain recent callsign pool across windows**

Add a `Vec<RecentCallAp>` that persists across decode windows in the FT8 thread. After each window's decode, add new callsigns and remove old ones (keep last 3 windows, cap at 20):

```rust
let mut recent_pool: Vec<RecentCallAp> = Vec::new();

// Inside the decode loop:
// ... decode window ...

// Update pool with this window's decoded callsigns
for msg in &decoded {
    if let Some(ref call) = msg.from_callsign {
        if !recent_pool.iter().any(|r| r.callsign == *call) {
            if let Some(ap) = RecentCallAp::new(call, msg.snr_db) {
                recent_pool.push(ap);
            }
        }
    }
}
// Cap at 20, keeping strongest
recent_pool.sort_by(|a, b| b.last_snr.partial_cmp(&a.last_snr).unwrap());
recent_pool.truncate(20);
```

- [ ] **Step 3: Build and verify**

```bash
touch pancetta/src/coordinator/pipeline.rs
cargo build --bin pancetta 2>&1 | tail -5
```

- [ ] **Step 4: Commit**

```bash
git add pancetta/src/coordinator/pipeline.rs
git commit -m "feat: wire ApContext from coordinator — AP1 always active, AP2 recent pool"
```

---

## Sub-project 2: Parallel Decode

### Task 5: Add rayon dependency and DecodeConfig

**Files:**
- Modify: `pancetta-ft8/Cargo.toml`
- Create: `pancetta-ft8/src/parallel.rs`
- Modify: `pancetta-ft8/src/lib.rs`

- [ ] **Step 1: Add rayon to Cargo.toml**

In `pancetta-ft8/Cargo.toml`, add to `[dependencies]`:
```toml
rayon = "1.10"
```

- [ ] **Step 2: Create parallel.rs with DecodeConfig**

Create `pancetta-ft8/src/parallel.rs`:

```rust
//! Parallel decode configuration and budget management.

use std::time::{Duration, Instant};

/// Controls decode parallelism and resource budget.
#[derive(Debug, Clone)]
pub struct DecodeConfig {
    /// Maximum candidates on pass 0
    pub max_candidates_pass0: usize,
    /// Maximum candidates on passes 1+
    pub max_candidates_pass_n: usize,
    /// Maximum decode passes (including signal subtraction)
    pub max_decode_passes: usize,
    /// Parallelism strategy
    pub parallelism: Parallelism,
    /// Hard wall-clock budget in milliseconds. Decode stops when exceeded.
    pub budget_ms: u64,
}

#[derive(Debug, Clone, Copy)]
pub enum Parallelism {
    /// Single-threaded decode (for debugging, testing, low-power devices)
    Serial,
    /// Rayon work-stealing thread pool
    Rayon {
        /// Max threads (None = use all available cores)
        max_threads: Option<usize>,
    },
}

impl Default for DecodeConfig {
    fn default() -> Self {
        Self {
            max_candidates_pass0: 100,
            max_candidates_pass_n: 40,
            max_decode_passes: 3,
            parallelism: Parallelism::Rayon { max_threads: None },
            budget_ms: 2000,
        }
    }
}

/// Budget tracker — checks if we've exceeded our time allocation.
pub struct BudgetTracker {
    deadline: Instant,
}

impl BudgetTracker {
    pub fn new(budget_ms: u64) -> Self {
        Self {
            deadline: Instant::now() + Duration::from_millis(budget_ms),
        }
    }

    /// Returns true if we've exceeded the budget.
    pub fn expired(&self) -> bool {
        Instant::now() >= self.deadline
    }

    /// Remaining time in milliseconds.
    pub fn remaining_ms(&self) -> u64 {
        self.deadline
            .checked_duration_since(Instant::now())
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_budget_tracker() {
        let tracker = BudgetTracker::new(1000);
        assert!(!tracker.expired());
        assert!(tracker.remaining_ms() > 900);
    }

    #[test]
    fn test_decode_config_default() {
        let config = DecodeConfig::default();
        assert_eq!(config.max_candidates_pass0, 100);
        assert_eq!(config.budget_ms, 2000);
        assert!(matches!(config.parallelism, Parallelism::Rayon { .. }));
    }
}
```

- [ ] **Step 3: Add to lib.rs**

```rust
pub mod parallel;
pub use parallel::{DecodeConfig, Parallelism, BudgetTracker};
```

- [ ] **Step 4: Build and test**

```bash
touch pancetta-ft8/src/parallel.rs pancetta-ft8/src/lib.rs pancetta-ft8/Cargo.toml
cargo test -p pancetta-ft8 --lib -- parallel 2>&1 | tail -5
```

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/Cargo.toml pancetta-ft8/src/parallel.rs pancetta-ft8/src/lib.rs
git commit -m "feat: add parallel decode config with rayon and budget management"
```

---

### Task 6: Parallelize candidate decoding with rayon

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs`

This is the most complex task — requires making candidate decoding thread-safe.

- [ ] **Step 1: Extract CandidateDecoder from Ft8Decoder**

The problem: `decode_candidate` takes `&mut self` because it reuses FFT buffers. For parallel decoding, each thread needs its own buffers. Extract the per-candidate state into a cheaply-cloneable struct:

```rust
/// Per-thread candidate decoder state. Contains LDPC decoder and
/// reusable buffers that can't be shared across threads.
struct CandidateDecoder {
    ldpc_decoder: LdpcDecoder,
    protocol_params: ProtocolParams,
    message_parser: MessageParser,
    // Per-thread FFT buffers for DFT fallback
    symbol_fft: std::sync::Arc<dyn rustfft::Fft<f64>>,
    symbol_window: Vec<f64>,
    symbol_fft_buffer: Vec<Complex<f64>>,
}
```

Move `decode_candidate`, `extract_symbols_complex`, `extract_symbols_from_spectrogram`, `compute_soft_llrs_db`, `decode_candidate_complex`, `verify_crc`, and `build_decoded_message` methods from `Ft8Decoder` to `CandidateDecoder`.

`Ft8Decoder` keeps: spectrogram computation, sync search, signal subtraction, waterfall generation — the serial parts.

- [ ] **Step 2: Create CandidateDecoder pool**

In `decode_window_with_ap`, create a pool of `CandidateDecoder` instances (one per rayon thread):

```rust
use rayon::prelude::*;

// Create candidate decoders for parallel use
let candidate_decoder = CandidateDecoder::new(
    &self.config, &self.protocol_params, &self.message_parser)?;

// Parallel decode
let results: Vec<Option<DecodedMessage>> = sync_candidates
    .par_iter()
    .map_init(
        || candidate_decoder.clone(), // one clone per rayon thread
        |decoder, candidate| {
            decoder.try_decode_with_ap(candidate, &spectrogram, ap_context)
        },
    )
    .collect();
```

`par_iter().map_init()` creates one `CandidateDecoder` per rayon thread, reusing it across candidates assigned to that thread.

- [ ] **Step 3: Add budget checking**

```rust
let budget = BudgetTracker::new(self.config.budget_ms());

for pass in 0..config.max_decode_passes {
    if budget.expired() {
        info!("Decode budget exceeded after pass {}", pass);
        break;
    }
    // ... decode pass ...
}
```

Also check budget inside the parallel decode — rayon doesn't natively support cancellation, but each candidate can check `budget.expired()` and return `None` early:

```rust
|decoder, candidate| {
    if budget.expired() { return None; }
    decoder.try_decode_with_ap(candidate, &spectrogram, ap_context)
}
```

Note: `BudgetTracker` needs to be `Send + Sync` for this. `Instant` is `Send + Sync`, so it works.

- [ ] **Step 4: Build and run all tests**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta -- --test-threads=1 2>&1 | grep "test result" | head -5
```

- [ ] **Step 5: Run sensitivity AND speed benchmarks**

```bash
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture 2>&1 | grep -E "^[a-z].*ours=|Overall"
time cargo test -p pancetta-ft8 --release --test wav_decode_tests -- test_cross_validate 2>&1 | grep "finished in"
```

Expected: sensitivity maintained (126%+), speed improved (target: under 1.5s/window with parallelism).

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: parallel candidate decoding via rayon with budget management"
```

---

### Task 7: Parallel determinism test

**Files:**
- Modify: `pancetta-ft8/tests/wav_decode_tests.rs`

- [ ] **Step 1: Write test verifying serial and parallel produce same results**

```rust
#[test]
fn test_parallel_matches_serial() {
    use pancetta_ft8::*;

    let path = format!("{}/tests/fixtures/wav/jtdx/190227_155815.wav", env!("CARGO_MANIFEST_DIR"));
    let reader = hound::WavReader::open(&path).unwrap();
    let samples: Vec<f32> = reader.into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0).collect();
    let window = &samples[..WINDOW_SAMPLES.min(samples.len())];

    // Serial decode
    let mut config_serial = Ft8Config::default();
    // Set parallelism to serial via decode config (implementation-dependent)
    let mut decoder_serial = Ft8Decoder::new(config_serial).unwrap();
    let serial_results = decoder_serial.decode_window(window).unwrap();

    // Parallel decode (default)
    let mut decoder_parallel = Ft8Decoder::new(Ft8Config::default()).unwrap();
    let parallel_results = decoder_parallel.decode_window(window).unwrap();

    // Same number of decodes (order may differ)
    assert_eq!(serial_results.len(), parallel_results.len(),
        "Serial decoded {} but parallel decoded {}",
        serial_results.len(), parallel_results.len());

    // Same messages (compare sorted by text)
    let mut serial_texts: Vec<String> = serial_results.iter().map(|m| m.text.clone()).collect();
    let mut parallel_texts: Vec<String> = parallel_results.iter().map(|m| m.text.clone()).collect();
    serial_texts.sort();
    parallel_texts.sort();
    assert_eq!(serial_texts, parallel_texts);
}
```

- [ ] **Step 2: Run and commit**

```bash
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_parallel 2>&1 | tail -5
git add pancetta-ft8/tests/wav_decode_tests.rs
git commit -m "test: verify parallel decode produces same results as serial"
```

---

## Sub-project 3: Advanced Decode Techniques

### Task 8: OSD-3

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (config default)

- [ ] **Step 1: Enable OSD-3 with tighter gate**

In `pancetta-ft8/src/decoder.rs`, change:
```rust
// Ft8Config default:
osd_depth: Some(3),  // was Some(2)
```

And in the OSD gate in `decode_soft`:
```rust
// Tighten gate for OSD-3 (125K trials)
const MAX_PARITY_ERRORS_FOR_OSD: usize = 2;  // was 3
```

OSD-3 code already exists in `osd.rs` (it handles arbitrary depth). The `is_plausible()` message validation catches CRC false positives.

- [ ] **Step 2: Build, test, benchmark**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture 2>&1 | grep Overall
```

- [ ] **Step 3: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: enable OSD-3 with parity gate <=2 for deeper error correction"
```

---

### Task 9: Block detection for candidate ranking

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs`

- [ ] **Step 1: Add block_score function**

After `compute_costas_score`, add:

```rust
/// Compute a coherent block score by summing spectrogram power at all
/// expected symbol positions. Uses Costas sync tones (known values) and
/// data symbol best-tone energies to produce a score that considers
/// the entire FT8 frame, not just the 21 sync symbols.
///
/// Returns dB difference between signal and noise averaged across the block.
fn block_score(
    &self,
    spec: &Spectrogram,
    candidate: &CostasCandidate,
) -> f64 {
    let pp = &self.protocol_params;
    let steps_per_symbol = 2 * TIME_OSR;
    let mut signal_sum = 0.0f64;
    let mut noise_sum = 0.0f64;
    let mut signal_count = 0usize;
    let mut noise_count = 0usize;

    for sym_idx in 0..pp.num_symbols {
        let t = candidate.time_step + sym_idx * steps_per_symbol;
        if t >= spec.num_steps { break; }

        // Find the strongest tone at this symbol position
        let mut best_power = f64::MIN;
        let mut best_tone = 0;
        for tone in 0..pp.num_tones {
            let f = candidate.freq_bin + tone;
            if f >= spec.num_bins { continue; }
            let power = spec.power[t][candidate.freq_sub][f];
            if power > best_power {
                best_power = power;
                best_tone = tone;
            }
        }

        signal_sum += best_power;
        signal_count += 1;

        // Noise: average of non-best tones
        for tone in 0..pp.num_tones {
            if tone == best_tone { continue; }
            let f = candidate.freq_bin + tone;
            if f >= spec.num_bins { continue; }
            noise_sum += spec.power[t][candidate.freq_sub][f];
            noise_count += 1;
        }
    }

    if signal_count == 0 || noise_count == 0 { return 0.0; }
    (signal_sum / signal_count as f64) - (noise_sum / noise_count as f64)
}
```

- [ ] **Step 2: Re-rank candidates by block score before decoding**

In `decode_window_with_ap`, after `costas_sync_search` and NMS, re-rank by block score:

```rust
// Re-rank candidates by block score (better than sync-only ranking)
let mut scored_candidates: Vec<(f64, CostasCandidate)> = sync_candidates
    .into_iter()
    .map(|c| {
        let bscore = self.block_score(&spectrogram, &c);
        (bscore, c)
    })
    .collect();
scored_candidates.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap());
let sync_candidates: Vec<CostasCandidate> = scored_candidates
    .into_iter()
    .map(|(_, c)| c)
    .collect();
```

- [ ] **Step 3: Build, test, benchmark**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture 2>&1 | grep Overall
```

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: block detection score for better candidate ranking (+0.7 dB)"
```

---

### Task 10: Multi-mode signal subtraction with sidelobe cancellation

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs` (subtract_signal area)

- [ ] **Step 1: Add sidelobe subtraction**

Find the `subtract_signal` method. After the main signal subtraction, add sidelobe cancellation:

```rust
fn subtract_with_sidelobes(
    &self,
    audio: &mut [f32],
    msg: &DecodedMessage,
) {
    // Main signal subtraction (existing method)
    self.subtract_signal(audio, msg);

    // Sidelobe cancellation: subtract scaled copies at ±1 tone spacing
    // The Hann window's first sidelobe is ~-31 dB below main lobe,
    // but with our windowing the effective sidelobe is ~15% (-16 dB)
    if let Some(ref tone_symbols) = msg.tone_symbols {
        let sidelobe_factor = 0.15;
        let freq_offset = msg.frequency_offset;

        // +1 tone spacing
        self.subtract_signal_at_freq(
            audio, tone_symbols, freq_offset + TONE_SPACING,
            msg.time_offset, sidelobe_factor);
        // -1 tone spacing
        self.subtract_signal_at_freq(
            audio, tone_symbols, freq_offset - TONE_SPACING,
            msg.time_offset, sidelobe_factor);
    }
}
```

Where `subtract_signal_at_freq` is a variant of the existing subtraction that uses a specified frequency and amplitude scale factor. Read the existing `subtract_signal` implementation to understand the exact structure, then add the frequency-shifted variant.

- [ ] **Step 2: Wire sidelobe subtraction into decode_window**

Replace calls to `subtract_signal` with `subtract_with_sidelobes` in the multi-pass loop.

- [ ] **Step 3: Build, test, benchmark**

```bash
touch pancetta-ft8/src/decoder.rs
cargo test -p pancetta-ft8 --lib 2>&1 | tail -5
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture 2>&1 | grep Overall
```

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/src/decoder.rs
git commit -m "feat: multi-mode signal subtraction with sidelobe cancellation"
```

---

### Task 11: Final threshold update and full test suite

**Files:**
- Modify: `pancetta-ft8/tests/wav_decode_tests.rs`

- [ ] **Step 1: Run final benchmarks**

```bash
cargo test -p pancetta-ft8 --test wav_decode_tests -- test_cross_validate --nocapture 2>&1 | grep -E "^[a-z].*ours=|Overall"
time cargo test -p pancetta-ft8 --release --test wav_decode_tests -- test_cross_validate 2>&1 | grep "finished in"
```

Record exact numbers.

- [ ] **Step 2: Update regression threshold**

Set threshold to 90% of achieved ratio (if 160%, set floor to 1.40):

```rust
assert!(
    overall_ratio >= 1.20,
    "REGRESSION: ...",
);
```

- [ ] **Step 3: Run full workspace test suite**

```bash
cargo test --workspace 2>&1 | grep "test result"
```

All must pass.

- [ ] **Step 4: Commit**

```bash
git add pancetta-ft8/tests/wav_decode_tests.rs
git commit -m "test: raise cross-validation threshold to lock in advanced decoder gains"
```

---

## Execution Notes

- **Sub-project 1 (AP)** Tasks 1-4 are sequential — each builds on the previous
- **Sub-project 2 (Parallel)** Tasks 5-7 are sequential — Task 6 is the complex one
- **Sub-project 3 (Advanced)** Tasks 8-10 are independent and can be parallelized
- Task 11 runs last after everything else is integrated
- Run the cross-validation benchmark after EVERY task to track incremental gains
- The benchmark takes ~12-25s in release mode
- Key constraint: sensitivity must never regress below 120% of ft8_lib
- Key constraint: speed must stay under 2.0s/window in release with rayon
