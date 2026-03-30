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
cargo build --release -p pancetta 2>&1 | tail -3

echo ""
echo "Running benchmark against: $WAV_DIR"
echo "======================================="

cargo run --release -p pancetta -- \
    benchmark-decode "$WAV_DIR" --format json \
    > "$OUTPUT_DIR/benchmark_${TIMESTAMP}.json" 2>/dev/null

# Also print text summary
cargo run --release -p pancetta -- \
    benchmark-decode "$WAV_DIR" 2>/dev/null

echo ""
echo "JSON results saved to: $OUTPUT_DIR/benchmark_${TIMESTAMP}.json"
