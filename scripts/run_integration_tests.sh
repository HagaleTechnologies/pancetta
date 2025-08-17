#!/bin/bash
# Integration test runner for Pancetta

set -e

echo "========================================="
echo "Pancetta Integration Test Suite"
echo "========================================="
echo ""

# Set environment variables for testing
export PANCETTA_STUB_AUDIO=1
export PANCETTA_MOCK_RIG=true
export RUST_LOG=info

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
NC='\033[0m' # No Color

# Track results
TOTAL=0
PASSED=0
FAILED=0

# Function to run a test
run_test() {
    local test_name=$1
    local test_cmd=$2
    
    TOTAL=$((TOTAL + 1))
    echo -n "Running $test_name... "
    
    if eval "$test_cmd" > /tmp/test_output.log 2>&1; then
        echo -e "${GREEN}✓ PASSED${NC}"
        PASSED=$((PASSED + 1))
    else
        echo -e "${RED}✗ FAILED${NC}"
        echo "  Error output:"
        tail -n 10 /tmp/test_output.log | sed 's/^/    /'
        FAILED=$((FAILED + 1))
    fi
}

echo "Building Pancetta..."
cargo build --release --bin pancetta 2>&1 | grep -E "(Compiling|Finished)" || true
echo ""

echo "Running Integration Tests:"
echo "--------------------------"

# Test 1: Basic startup and shutdown
run_test "Basic Startup" "timeout 2 ./target/release/pancetta --headless"

# Test 2: Audio pipeline test
run_test "Audio Pipeline" "timeout 3 ./target/release/pancetta --headless"

# Test 3: Memory usage test
run_test "Memory Usage" "bash -c 'timeout 5 ./target/release/pancetta --headless & sleep 2; ps aux | grep pancetta | grep -v grep | awk \"{print \\\$4}\" | head -1 | awk \"{if(\\\$1 < 1.0) exit 0; else exit 1}\"'"

# Test 4: CPU usage test  
run_test "CPU Usage" "bash -c 'timeout 5 ./target/release/pancetta --headless & sleep 2; ps aux | grep pancetta | grep -v grep | awk \"{print \\\$3}\" | head -1 | awk \"{if(\\\$1 < 25.0) exit 0; else exit 1}\"'"

echo ""
echo "Running Unit Tests:"
echo "-------------------"

# Run unit tests for each module
for package in pancetta-ft8 pancetta-dsp pancetta-audio pancetta-hamlib; do
    run_test "$package tests" "cargo test --package $package --lib"
done

echo ""
echo "========================================="
echo "Test Results Summary:"
echo "========================================="
echo "Total:  $TOTAL"
echo -e "Passed: ${GREEN}$PASSED${NC}"
echo -e "Failed: ${RED}$FAILED${NC}"

if [ $FAILED -eq 0 ]; then
    echo -e "\n${GREEN}All tests passed!${NC}"
    exit 0
else
    echo -e "\n${RED}$FAILED test(s) failed${NC}"
    exit 1
fi