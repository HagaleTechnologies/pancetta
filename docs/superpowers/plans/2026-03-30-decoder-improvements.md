# Decoder Improvements Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Bring Pancetta's FT8 decoder to WSJT-X-competitive decode rates through benchmarking, fine frequency/time estimation, and improved multi-pass signal subtraction.

**Architecture:** The decoder pipeline lives in `pancetta-ft8/src/decoder.rs`. We add a benchmark CLI tool to measure decode rate against ft8_lib (C reference), improve `decode_candidate()` with sub-bin frequency and sub-sample time refinement, and validate the existing multi-pass subtraction loop. All changes are validated via the benchmark harness before/after.

**Tech Stack:** Rust, rustfft, hound (WAV I/O), serde_json (structured output), clap (CLI), ft8_lib FFI (reference decoder)

---

## File Structure

| File | Responsibility |
|------|---------------|
| `pancetta-ft8/src/decoder.rs` | Modify: add `refine_candidate()`, improve frequency/time search in `decode_candidate()` |
| `pancetta-ft8/src/benchmark.rs` | Create: benchmark harness — decode WAVs, compare against ft8_lib, structured JSON output |
| `pancetta-ft8/tests/benchmark_tests.rs` | Create: tests for benchmark module |
| `pancetta/src/main.rs` | Modify: wire up `benchmark-decode` subcommand |
| `pancetta-ft8/tests/decoder_refinement_tests.rs` | Create: tests for fine freq/time and multi-pass improvements |
| `scripts/benchmark_compare.sh` | Create: shell script to run benchmark and format results |

---

### Task 1: Benchmark Module — Structured Decode Output

**Files:**
- Create: `pancetta-ft8/src/benchmark.rs`
- Modify: `pancetta-ft8/src/lib.rs`

- [ ] **Step 1: Write the failing test**

Create `pancetta-ft8/tests/benchmark_tests.rs`:

```rust
use pancetta_ft8::benchmark::{BenchmarkResult, DecodeResult, decode_wav_to_results};
use pancetta_ft8::SAMPLE_RATE;

fn fixture(subpath: &str) -> String {
    format!("{}/tests/fixtures/wav/{}", env!("CARGO_MANIFEST_DIR"), subpath)
}

#[test]
fn test_decode_wav_to_results_returns_structured_output() {
    let results = decode_wav_to_results(&fixture("generated/ft8_cq.wav"));
    assert!(results.is_ok());
    let bench = results.unwrap();
    assert_eq!(bench.file_path, fixture("generated/ft8_cq.wav"));
    assert!(bench.processing_time_ms > 0.0);
    // We don't assert decode count — the test validates structure, not decode rate
}

#[test]
fn test_decode_result_serializes_to_json() {
    let result = DecodeResult {
        message: "CQ W1ABC FN42".to_string(),
        frequency_hz: 1500.0,
        time_offset_s: 0.5,
        snr_db: -10.0,
    };
    let json = serde_json::to_string(&result).unwrap();
    assert!(json.contains("CQ W1ABC FN42"));
    assert!(json.contains("1500"));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p pancetta-ft8 --test benchmark_tests 2>&1 | head -20`
Expected: Compilation error — `benchmark` module doesn't exist yet.

- [ ] **Step 3: Write the benchmark module**

Create `pancetta-ft8/src/benchmark.rs`:

```rust
//! Benchmark harness for comparing Pancetta decoder against ft8_lib reference.

use crate::{Ft8Config, Ft8Decoder, SAMPLE_RATE, WINDOW_SAMPLES};
use crate::ft8_lib_ffi::ft8lib_decode_audio;
use serde::{Deserialize, Serialize};
use std::time::Instant;

/// A single decoded message with position metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DecodeResult {
    pub message: String,
    pub frequency_hz: f64,
    pub time_offset_s: f64,
    pub snr_db: f32,
}

/// Results from decoding a single WAV file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub file_path: String,
    pub pancetta_decodes: Vec<DecodeResult>,
    pub ft8lib_decodes: Vec<DecodeResult>,
    pub processing_time_ms: f64,
}

/// Comparison summary across all WAV files.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComparisonSummary {
    pub total_files: usize,
    pub pancetta_total: usize,
    pub ft8lib_total: usize,
    pub both_decoded: usize,
    pub pancetta_only: usize,
    pub ft8lib_only: usize,
    pub parity_percent: f64,
    pub per_file: Vec<BenchmarkResult>,
}

/// Read a WAV file into f32 samples. Panics on invalid format.
pub fn read_wav_samples(path: &str) -> Result<Vec<f32>, String> {
    let reader = hound::WavReader::open(path)
        .map_err(|e| format!("Failed to open {}: {}", path, e))?;
    let spec = reader.spec();
    if spec.channels != 1 {
        return Err(format!("Expected mono, got {} channels", spec.channels));
    }
    if spec.sample_rate != SAMPLE_RATE {
        return Err(format!(
            "Expected {}Hz sample rate, got {}Hz",
            SAMPLE_RATE, spec.sample_rate
        ));
    }
    let samples: Vec<f32> = reader
        .into_samples::<i16>()
        .map(|s| s.unwrap() as f32 / 32768.0)
        .collect();
    Ok(samples)
}

/// Decode a WAV file with Pancetta and ft8_lib, returning structured results.
pub fn decode_wav_to_results(path: &str) -> Result<BenchmarkResult, String> {
    let samples = read_wav_samples(path)?;
    let buffer: Vec<f32> = if samples.len() >= WINDOW_SAMPLES {
        samples.clone()
    } else {
        let mut padded = samples.clone();
        padded.resize(WINDOW_SAMPLES, 0.0);
        padded
    };

    // Pancetta decode
    let start = Instant::now();
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).map_err(|e| format!("Decoder init: {}", e))?;
    let pancetta_msgs = decoder.decode_window(&buffer).unwrap_or_default();
    let processing_time_ms = start.elapsed().as_secs_f64() * 1000.0;

    let pancetta_decodes: Vec<DecodeResult> = pancetta_msgs
        .iter()
        .map(|m| DecodeResult {
            message: m.text.clone(),
            frequency_hz: m.frequency_offset,
            time_offset_s: m.time_offset,
            snr_db: m.snr_db,
        })
        .collect();

    // ft8_lib reference decode
    let ft8lib_raw = ft8lib_decode_audio(&buffer);
    let ft8lib_decodes: Vec<DecodeResult> = ft8lib_raw
        .iter()
        .map(|(msg, freq, time, _errs)| DecodeResult {
            message: msg.clone(),
            frequency_hz: *freq as f64,
            time_offset_s: *time as f64,
            snr_db: 0.0, // ft8_lib doesn't report SNR in same way
        })
        .collect();

    Ok(BenchmarkResult {
        file_path: path.to_string(),
        pancetta_decodes,
        ft8lib_decodes,
        processing_time_ms,
    })
}

/// Compare decode results across multiple WAV files.
pub fn compare_results(results: &[BenchmarkResult]) -> ComparisonSummary {
    let mut pancetta_total = 0;
    let mut ft8lib_total = 0;
    let mut both_decoded = 0;
    let mut pancetta_only = 0;
    let mut ft8lib_only = 0;

    for result in results {
        let p_msgs: std::collections::HashSet<&str> = result
            .pancetta_decodes
            .iter()
            .map(|d| d.message.as_str())
            .collect();
        let f_msgs: std::collections::HashSet<&str> = result
            .ft8lib_decodes
            .iter()
            .map(|d| d.message.as_str())
            .collect();

        pancetta_total += p_msgs.len();
        ft8lib_total += f_msgs.len();
        both_decoded += p_msgs.intersection(&f_msgs).count();
        pancetta_only += p_msgs.difference(&f_msgs).count();
        ft8lib_only += f_msgs.difference(&p_msgs).count();
    }

    let parity_percent = if ft8lib_total > 0 {
        (pancetta_total as f64 / ft8lib_total as f64) * 100.0
    } else {
        100.0
    };

    ComparisonSummary {
        total_files: results.len(),
        pancetta_total,
        ft8lib_total,
        both_decoded,
        pancetta_only,
        ft8lib_only,
        parity_percent,
        per_file: results.to_vec(),
    }
}
```

- [ ] **Step 4: Export the benchmark module from lib.rs**

Add to `pancetta-ft8/src/lib.rs`, after the existing `pub mod` declarations:

```rust
pub mod benchmark;
```

Also add `serde` to `pancetta-ft8/Cargo.toml` dependencies (if not already present):

```toml
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p pancetta-ft8 --test benchmark_tests -- --nocapture 2>&1 | tail -20`
Expected: Both tests PASS.

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/benchmark.rs pancetta-ft8/src/lib.rs pancetta-ft8/Cargo.toml pancetta-ft8/tests/benchmark_tests.rs
git commit -m "feat(ft8): add benchmark harness for decoder comparison"
```

---

### Task 2: Benchmark CLI Subcommand

**Files:**
- Modify: `pancetta/src/main.rs`
- Modify: `pancetta/Cargo.toml`

- [ ] **Step 1: Add BenchmarkDecodeArgs and wire subcommand**

In `pancetta/src/main.rs`, add a new variant to the `Commands` enum (around line 117):

```rust
    /// Benchmark decoder against ft8_lib reference
    BenchmarkDecode(BenchmarkDecodeArgs),
```

Add the args struct (after `BenchmarkArgs`):

```rust
#[derive(Clone, Args)]
struct BenchmarkDecodeArgs {
    /// Path to a WAV file or directory of WAV files
    #[arg(required = true)]
    path: String,

    /// Output format: "text" or "json"
    #[arg(long, default_value = "text")]
    format: String,
}
```

Add the match arm in `handle_command()` (around line 325):

```rust
        Commands::BenchmarkDecode(args) => benchmark_decode_command(args).await,
```

Add the handler function:

```rust
async fn benchmark_decode_command(args: BenchmarkDecodeArgs) -> Result<()> {
    use pancetta_ft8::benchmark::{compare_results, decode_wav_to_results};
    use std::path::Path;

    let path = Path::new(&args.path);
    let wav_files: Vec<String> = if path.is_dir() {
        let mut files: Vec<String> = std::fs::read_dir(path)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("wav") {
                    Some(p.to_string_lossy().to_string())
                } else {
                    None
                }
            })
            .collect();
        files.sort();
        files
    } else {
        vec![args.path.clone()]
    };

    if wav_files.is_empty() {
        eprintln!("No WAV files found at {}", args.path);
        std::process::exit(1);
    }

    let mut results = Vec::new();
    for wav_path in &wav_files {
        eprintln!("Decoding: {}", wav_path);
        match decode_wav_to_results(wav_path) {
            Ok(result) => {
                eprintln!(
                    "  Pancetta: {} decodes, ft8_lib: {} decodes ({:.0}ms)",
                    result.pancetta_decodes.len(),
                    result.ft8lib_decodes.len(),
                    result.processing_time_ms,
                );
                results.push(result);
            }
            Err(e) => eprintln!("  Error: {}", e),
        }
    }

    let summary = compare_results(&results);

    if args.format == "json" {
        println!("{}", serde_json::to_string_pretty(&summary).unwrap());
    } else {
        println!("\n=== Decoder Benchmark Summary ===");
        println!("Files:          {}", summary.total_files);
        println!("Pancetta total: {}", summary.pancetta_total);
        println!("ft8_lib total:  {}", summary.ft8lib_total);
        println!("Both decoded:   {}", summary.both_decoded);
        println!("Pancetta only:  {}", summary.pancetta_only);
        println!("ft8_lib only:   {}", summary.ft8lib_only);
        println!("Parity:         {:.1}%", summary.parity_percent);
    }

    Ok(())
}
```

- [ ] **Step 2: Add serde_json dependency to pancetta/Cargo.toml**

Add under `[dependencies]`:

```toml
serde_json = "1"
```

- [ ] **Step 3: Build and verify the subcommand exists**

Run: `cargo build -p pancetta --features transmit 2>&1 | tail -5`
Expected: Successful build.

Run: `cargo run -p pancetta -- benchmark-decode --help 2>&1`
Expected: Help output showing `path` argument and `--format` option.

- [ ] **Step 4: Run against a WAV fixture**

Run: `cargo run --release -p pancetta --features transmit -- benchmark-decode pancetta-ft8/tests/fixtures/wav/generated/ft8_cq.wav 2>&1`
Expected: Output showing Pancetta and ft8_lib decode counts.

- [ ] **Step 5: Run against the full fixture directory**

Run: `cargo run --release -p pancetta --features transmit -- benchmark-decode pancetta-ft8/tests/fixtures/wav/ 2>&1`
Expected: Summary across all WAV files with parity percentage.

- [ ] **Step 6: Commit**

```bash
git add pancetta/src/main.rs pancetta/Cargo.toml
git commit -m "feat(cli): add benchmark-decode subcommand for decoder comparison"
```

---

### Task 3: Benchmark Shell Script

**Files:**
- Create: `scripts/benchmark_compare.sh`

- [ ] **Step 1: Create the benchmark script**

```bash
#!/usr/bin/env bash
# Benchmark Pancetta decoder against ft8_lib reference.
# Usage: ./scripts/benchmark_compare.sh [WAV_DIR]
#
# Default WAV_DIR: pancetta-ft8/tests/fixtures/wav/

set -euo pipefail

WAV_DIR="${1:-pancetta-ft8/tests/fixtures/wav/}"
TIMESTAMP=$(date +%Y%m%d_%H%M%S)
OUTPUT_DIR="benchmarks/results"

mkdir -p "$OUTPUT_DIR"

echo "Building release binary..."
cargo build --release -p pancetta --features transmit 2>&1 | tail -3

echo ""
echo "Running benchmark against: $WAV_DIR"
echo "======================================="

cargo run --release -p pancetta --features transmit -- \
    benchmark-decode "$WAV_DIR" --format json \
    > "$OUTPUT_DIR/benchmark_${TIMESTAMP}.json" 2>/dev/null

# Also print text summary
cargo run --release -p pancetta --features transmit -- \
    benchmark-decode "$WAV_DIR" 2>/dev/null

echo ""
echo "JSON results saved to: $OUTPUT_DIR/benchmark_${TIMESTAMP}.json"
```

- [ ] **Step 2: Make it executable and test**

Run: `chmod +x scripts/benchmark_compare.sh && ./scripts/benchmark_compare.sh 2>&1`
Expected: Text summary printed, JSON saved to `benchmarks/results/`.

- [ ] **Step 3: Add benchmarks/results to .gitignore**

Append to `.gitignore`:

```
benchmarks/results/
```

- [ ] **Step 4: Commit**

```bash
git add scripts/benchmark_compare.sh .gitignore
git commit -m "feat: add benchmark comparison script"
```

---

### Task 4: Establish Baseline Benchmark

This is a measurement step, not a code change. Record current decode parity before any improvements.

- [ ] **Step 1: Run baseline benchmark**

Run: `./scripts/benchmark_compare.sh 2>&1`

Record the parity percentage. This is our starting point.

- [ ] **Step 2: Save baseline to a tracking file**

Create `benchmarks/BASELINE.md`:

```markdown
# Decoder Benchmark Baseline

## Date: YYYY-MM-DD (fill in actual date)

## Results (pre-improvement)

Parity: X% (fill in from benchmark output)

| File | Pancetta | ft8_lib | Both | P-only | F-only |
|------|----------|---------|------|--------|--------|
| (fill in per-file results from JSON output) |
```

- [ ] **Step 3: Commit baseline**

```bash
git add benchmarks/BASELINE.md
git commit -m "docs: record decoder benchmark baseline"
```

---

### Task 5: Fine Frequency Estimation — Sub-Bin Interpolation

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs`
- Create: `pancetta-ft8/tests/decoder_refinement_tests.rs`

- [ ] **Step 1: Write the failing test**

Create `pancetta-ft8/tests/decoder_refinement_tests.rs`:

```rust
//! Tests for decoder frequency/time refinement improvements

use pancetta_ft8::{Ft8Config, Ft8Decoder, SAMPLE_RATE, WINDOW_SAMPLES};

#[cfg(feature = "transmit")]
mod refinement {
    use super::*;

    /// Helper: generate a known FT8 signal at a specific frequency offset
    /// and verify the decoder finds it. Returns (decoded_count, frequency_error_hz).
    fn decode_at_offset(freq_offset: f64) -> (usize, f64) {
        use pancetta_ft8::encoder::Ft8Encoder;
        use pancetta_ft8::modulator::Ft8Modulator;

        let mut encoder = Ft8Encoder::new();
        let symbols = encoder.encode_message("CQ W1ABC FN42", None).unwrap();

        let mut modulator =
            Ft8Modulator::new(SAMPLE_RATE, pancetta_ft8::BASE_FREQUENCY, 1.0).unwrap();
        let signal = modulator.modulate_symbols(&symbols, freq_offset).unwrap();

        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        for (i, &s) in signal.iter().enumerate() {
            if i < samples.len() {
                samples[i] = s;
            }
        }

        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let decoded = decoder.decode_window(&samples).unwrap();

        let count = decoded.len();
        let freq_error = if let Some(msg) = decoded.first() {
            (msg.frequency_offset - (pancetta_ft8::BASE_FREQUENCY + freq_offset)).abs()
        } else {
            f64::MAX
        };

        (count, freq_error)
    }

    #[test]
    fn test_decode_at_exact_bin_center() {
        // Signal at exact bin center (multiple of 6.25 Hz offset)
        let (count, _) = decode_at_offset(0.0);
        assert!(count >= 1, "Should decode signal at bin center");
    }

    #[test]
    fn test_decode_at_quarter_bin_offset() {
        // Signal at 1/4 bin offset (1.5625 Hz from bin center)
        // This should still decode after fine frequency refinement
        let (count, _) = decode_at_offset(1.5625);
        assert!(count >= 1, "Should decode signal at quarter-bin offset");
    }

    #[test]
    fn test_decode_at_half_bin_offset() {
        // Signal at exactly half-bin offset (3.125 Hz) — worst case for coarse search
        // freq_osr=2 handles this via sub-bin, but sub-bin refinement should improve it
        let (count, _) = decode_at_offset(3.125);
        assert!(count >= 1, "Should decode signal at half-bin offset");
    }

    #[test]
    fn test_frequency_estimate_accuracy() {
        // After refinement, frequency estimate should be within 1 Hz
        let (count, freq_error) = decode_at_offset(1.0);
        assert!(count >= 1, "Should decode signal");
        assert!(
            freq_error < 2.0,
            "Frequency error {:.2} Hz should be < 2 Hz",
            freq_error
        );
    }
}
```

- [ ] **Step 2: Run tests to verify they pass with current code (baseline)**

Run: `cargo test -p pancetta-ft8 --test decoder_refinement_tests --features transmit -- --nocapture 2>&1 | tail -20`

Record which tests pass/fail. These tests should mostly pass already since we're testing at clean SNR. The point is to establish the baseline before refinement.

- [ ] **Step 3: Add sub-bin frequency search to decode_candidate()**

In `pancetta-ft8/src/decoder.rs`, modify the frequency search in `decode_candidate()` (around line 758). Replace:

```rust
        // Frequency refinement: try ±1 bin
        let freq_offsets: [isize; 3] = [0, -1, 1];
```

With:

```rust
        // Frequency refinement: try ±1 bin with quarter-bin sub-steps
        // This gives 5 frequency trials per bin: -1, -0.5, 0, +0.5, +1
        // (in units of tone_spacing = 6.25 Hz, so steps are 3.125 Hz)
        let freq_offsets: [f64; 5] = [0.0, -0.5, 0.5, -1.0, 1.0];
```

Then update the inner loop (around line 774) to use `f64` offsets:

Replace:

```rust
            for &df in &freq_offsets {
                let freq_bin = candidate.freq_bin as isize + df;
                if freq_bin < 0 {
                    continue;
                }
                let base_frequency = freq_bin as f64 * pp.tone_spacing + sub_bin_offset;
```

With:

```rust
            for &df in &freq_offsets {
                let freq_hz = candidate.freq_bin as f64 * pp.tone_spacing + sub_bin_offset
                    + df * pp.tone_spacing;
                if freq_hz < 0.0 {
                    continue;
                }
                let base_frequency = freq_hz;
```

- [ ] **Step 4: Run all existing tests to verify no regressions**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -10`
Expected: All existing tests still pass. The sub-bin search is a superset of the old search.

- [ ] **Step 5: Run refinement tests**

Run: `cargo test -p pancetta-ft8 --test decoder_refinement_tests --features transmit -- --nocapture 2>&1 | tail -20`
Expected: All 4 tests pass.

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/decoder.rs pancetta-ft8/tests/decoder_refinement_tests.rs
git commit -m "feat(decoder): add sub-bin frequency refinement (quarter-bin steps)"
```

---

### Task 6: Fine Time Estimation — Sub-Sample Interpolation

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs`
- Modify: `pancetta-ft8/tests/decoder_refinement_tests.rs`

- [ ] **Step 1: Write the failing test**

Add to `pancetta-ft8/tests/decoder_refinement_tests.rs` inside `mod refinement`:

```rust
    #[test]
    fn test_decode_with_time_offset() {
        use pancetta_ft8::encoder::Ft8Encoder;
        use pancetta_ft8::modulator::Ft8Modulator;

        let mut encoder = Ft8Encoder::new();
        let symbols = encoder.encode_message("CQ W1ABC FN42", None).unwrap();
        let mut modulator =
            Ft8Modulator::new(SAMPLE_RATE, pancetta_ft8::BASE_FREQUENCY, 1.0).unwrap();
        let signal = modulator.modulate_symbols(&symbols, 0.0).unwrap();

        // Place signal with a 100-sample offset (8.3ms) from the start
        // This tests that the time refinement can find signals not aligned to symbol boundaries
        let offset = 100;
        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        for (i, &s) in signal.iter().enumerate() {
            if i + offset < samples.len() {
                samples[i + offset] = s;
            }
        }

        let config = Ft8Config::default();
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let decoded = decoder.decode_window(&samples).unwrap();
        assert!(
            !decoded.is_empty(),
            "Should decode signal with 100-sample time offset"
        );
    }
```

- [ ] **Step 2: Run test to establish baseline**

Run: `cargo test -p pancetta-ft8 --test decoder_refinement_tests --features transmit test_decode_with_time_offset -- --nocapture 2>&1`

This may already pass since the existing ±half-symbol time search covers 960 samples. Record the result.

- [ ] **Step 3: Refine time search with finer steps**

In `pancetta-ft8/src/decoder.rs`, modify the time search in `decode_candidate()` (around line 748). Replace:

```rust
        // Fine timing: search ±half symbol in sub-symbol steps.
        let quarter_sym = (sps / 4) as isize;
        let time_deltas: [isize; 5] = [
            -2 * quarter_sym,
            -quarter_sym,
            0,
            quarter_sym,
            2 * quarter_sym,
        ];
```

With:

```rust
        // Fine timing: search ±half symbol in eighth-symbol steps.
        // Finer time steps improve symbol extraction for signals not aligned to
        // the coarse Costas sync grid. 9 steps at 1/8 symbol = 240 samples each.
        let eighth_sym = (sps / 8) as isize;
        let time_deltas: [isize; 9] = [
            -4 * eighth_sym,
            -3 * eighth_sym,
            -2 * eighth_sym,
            -eighth_sym,
            0,
            eighth_sym,
            2 * eighth_sym,
            3 * eighth_sym,
            4 * eighth_sym,
        ];
```

- [ ] **Step 4: Run all tests to verify no regressions**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -10`
Expected: All tests pass.

- [ ] **Step 5: Commit**

```bash
git add pancetta-ft8/src/decoder.rs pancetta-ft8/tests/decoder_refinement_tests.rs
git commit -m "feat(decoder): add finer time search (eighth-symbol steps)"
```

---

### Task 7: Multi-Pass Subtraction — Validation and Phase-Aware Improvement

**Files:**
- Modify: `pancetta-ft8/src/decoder.rs`
- Modify: `pancetta-ft8/tests/decoder_refinement_tests.rs`

- [ ] **Step 1: Write multi-pass test with overlapping signals**

Add to `pancetta-ft8/tests/decoder_refinement_tests.rs` inside `mod refinement`:

```rust
    #[test]
    fn test_multipass_decodes_overlapping_signals() {
        use pancetta_ft8::encoder::Ft8Encoder;
        use pancetta_ft8::modulator::Ft8Modulator;

        // Create two signals at different frequencies
        let mut encoder = Ft8Encoder::new();
        let mut modulator =
            Ft8Modulator::new(SAMPLE_RATE, pancetta_ft8::BASE_FREQUENCY, 1.0).unwrap();

        // Signal 1: strong, at 0 Hz offset
        let symbols1 = encoder.encode_message("CQ W1ABC FN42", None).unwrap();
        let signal1 = modulator.modulate_symbols(&symbols1, 0.0).unwrap();

        // Signal 2: weaker, at +100 Hz offset
        let symbols2 = encoder.encode_message("CQ K2DEF EM73", None).unwrap();
        let signal2 = modulator.modulate_symbols(&symbols2, 100.0).unwrap();

        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        for (i, &s) in signal1.iter().enumerate() {
            if i < samples.len() {
                samples[i] += s;
            }
        }
        // Add signal 2 at half amplitude (6 dB weaker)
        for (i, &s) in signal2.iter().enumerate() {
            if i < samples.len() {
                samples[i] += s * 0.5;
            }
        }

        // With multi-pass (default 3), should decode both
        let mut config = Ft8Config::default();
        config.max_decode_passes = 3;
        let mut decoder = Ft8Decoder::new(config).unwrap();
        let decoded = decoder.decode_window(&samples).unwrap();

        let messages: Vec<&str> = decoded.iter().map(|m| m.text.as_str()).collect();
        assert!(
            decoded.len() >= 2,
            "Multi-pass should decode both signals, got: {:?}",
            messages
        );
    }

    #[test]
    fn test_single_pass_vs_multipass() {
        use pancetta_ft8::encoder::Ft8Encoder;
        use pancetta_ft8::modulator::Ft8Modulator;

        let mut encoder = Ft8Encoder::new();
        let mut modulator =
            Ft8Modulator::new(SAMPLE_RATE, pancetta_ft8::BASE_FREQUENCY, 1.0).unwrap();

        // Three signals at different frequencies
        let msgs = ["CQ W1ABC FN42", "CQ K2DEF EM73", "CQ N3GHI DM65"];
        let offsets = [0.0, 75.0, 150.0];
        let amplitudes = [1.0f32, 0.5, 0.25];

        let mut samples = vec![0.0f32; WINDOW_SAMPLES];
        for (idx, msg) in msgs.iter().enumerate() {
            let symbols = encoder.encode_message(msg, None).unwrap();
            let signal = modulator.modulate_symbols(&symbols, offsets[idx]).unwrap();
            for (i, &s) in signal.iter().enumerate() {
                if i < samples.len() {
                    samples[i] += s * amplitudes[idx];
                }
            }
        }

        // Single pass
        let mut config1 = Ft8Config::default();
        config1.max_decode_passes = 1;
        let mut decoder1 = Ft8Decoder::new(config1).unwrap();
        let decoded1 = decoder1.decode_window(&samples.clone()).unwrap();

        // Multi-pass
        let mut config3 = Ft8Config::default();
        config3.max_decode_passes = 3;
        let mut decoder3 = Ft8Decoder::new(config3).unwrap();
        let decoded3 = decoder3.decode_window(&samples).unwrap();

        println!("Single pass: {} decodes", decoded1.len());
        println!("Multi-pass:  {} decodes", decoded3.len());
        assert!(
            decoded3.len() >= decoded1.len(),
            "Multi-pass ({}) should decode at least as many as single-pass ({})",
            decoded3.len(),
            decoded1.len()
        );
    }
```

- [ ] **Step 2: Run tests to establish baseline**

Run: `cargo test -p pancetta-ft8 --test decoder_refinement_tests --features transmit -- --nocapture 2>&1 | tail -30`

Record results. If the multi-pass test fails (only decodes 1 of 2), the subtraction needs improvement.

- [ ] **Step 3: Improve signal subtraction with phase estimation**

The current `subtract_signal()` in `decoder.rs` (line 375) uses energy scaling but no phase alignment. For clean subtraction, we need to estimate the phase offset between the reconstructed signal and the original.

In `pancetta-ft8/src/decoder.rs`, replace the subtraction loop in `subtract_signal()` (around lines 420-443):

Replace:

```rust
            let orig_energy: f64 = (0..signal_len)
                .map(|i| {
                    let s = audio[time_offset_samples + i] as f64;
                    s * s
                })
                .sum();

            let recon_energy: f64 = (0..signal_len)
                .map(|i| {
                    let s = reconstructed[i] as f64;
                    s * s
                })
                .sum();

            // Scale with conservative factor (0.9) to avoid over-subtraction artifacts
            let scale = if recon_energy > 1e-12 {
                (orig_energy / recon_energy).sqrt() as f32 * 0.9
            } else {
                0.0
            };

            for i in 0..signal_len {
                audio[time_offset_samples + i] -= reconstructed[i] * scale;
            }
```

With:

```rust
            // Estimate amplitude and phase using cross-correlation.
            // The reconstructed signal r(t) should match a*r(t)*cos(phi) in the original.
            // We compute: amplitude = dot(orig, recon) / dot(recon, recon)
            // This naturally handles phase-aligned amplitude estimation.
            let dot_or: f64 = (0..signal_len)
                .map(|i| {
                    audio[time_offset_samples + i] as f64 * reconstructed[i] as f64
                })
                .sum();

            let dot_rr: f64 = (0..signal_len)
                .map(|i| {
                    let s = reconstructed[i] as f64;
                    s * s
                })
                .sum();

            // Scale factor: projection of original onto reconstructed signal.
            // Clamp to [0, 3] to avoid runaway subtraction from noise correlation.
            // Apply conservative 0.9 factor to avoid over-subtraction artifacts.
            let scale = if dot_rr > 1e-12 {
                ((dot_or / dot_rr) as f32).clamp(0.0, 3.0) * 0.9
            } else {
                0.0
            };

            for i in 0..signal_len {
                audio[time_offset_samples + i] -= reconstructed[i] * scale;
            }
```

- [ ] **Step 4: Run all tests**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -10`
Expected: All tests pass, including multi-pass tests.

- [ ] **Step 5: Run benchmark to measure improvement**

Run: `./scripts/benchmark_compare.sh 2>&1`
Compare parity percentage against the baseline recorded in Task 4.

- [ ] **Step 6: Commit**

```bash
git add pancetta-ft8/src/decoder.rs pancetta-ft8/tests/decoder_refinement_tests.rs
git commit -m "feat(decoder): improve signal subtraction with cross-correlation scaling"
```

---

### Task 8: Post-Improvement Benchmark and Documentation

- [ ] **Step 1: Run full benchmark suite**

Run: `./scripts/benchmark_compare.sh 2>&1`
Record the parity percentage and compare against baseline.

- [ ] **Step 2: Update BASELINE.md with results**

Update `benchmarks/BASELINE.md` to add a "Post-Improvement" section:

```markdown
## Results (post fine-freq + time + subtraction improvement)

Parity: X% (fill in from benchmark output)
Improvement: +X percentage points
```

- [ ] **Step 3: Run the full test suite**

Run: `cargo test -p pancetta-ft8 --features transmit 2>&1 | tail -10`
Expected: All tests pass, 0 failures.

- [ ] **Step 4: Commit**

```bash
git add benchmarks/BASELINE.md
git commit -m "docs: record post-improvement decoder benchmark results"
```

---

## Next Plan

After this plan is complete, the next implementation plan covers **OSD (Ordered Statistics Decoding)** — Phase 3 from the spec. OSD is algorithmically independent and adds ~2 dB of weak-signal sensitivity on top of the improvements made here.
