#!/bin/bash
set -e

echo "Testing Mode implementation compilation..."
cd /Users/thagale/Code/pancetta/pancetta-core

# Build the library
echo "Building pancetta-core..."
cargo build --lib

# Run the tests for the new mode module
echo -e "\nRunning mode_v2 tests..."
cargo test mode_v2 -- --nocapture

# Run the threading example
echo -e "\nRunning threading example..."
cargo run --example mode_threading

echo -e "\nAll tests passed successfully!"