#!/bin/bash
# Test script for Pancetta audio pipeline

echo "=== Pancetta Audio Pipeline Test Suite ==="
echo

# Set environment for testing
export RUST_LOG=debug
export PANCETTA_STUB_AUDIO=1

echo "1. Running unit tests..."
cargo test --package pancetta-ft8 --lib 2>&1 | grep -E "test result:|passed"
cargo test --package pancetta-audio --lib 2>&1 | grep -E "test result:|passed"
cargo test --package pancetta-dsp --lib 2>&1 | grep -E "test result:|passed"

echo
echo "2. Running integration tests..."
cargo test --package pancetta --test integration 2>&1 | grep -E "test result:|passed"

echo
echo "3. Testing FT8 signal generator..."
cargo test --package pancetta-ft8 --test test_signal_generator 2>&1 | grep -E "test result:|passed"

echo
echo "4. Testing audio pipeline flow..."
cargo test --package pancetta --test audio_pipeline_test test_audio_data_flow 2>&1 | grep -E "test.*ok|PASSED"

echo
echo "5. Testing FT8 window accumulation..."
cargo test --package pancetta --test audio_pipeline_test test_ft8_window_accumulation 2>&1 | grep -E "test.*ok|PASSED"

echo
echo "=== Test Summary ==="
echo "All core pipeline tests completed."
echo "Run 'cargo test --workspace' for full test suite."