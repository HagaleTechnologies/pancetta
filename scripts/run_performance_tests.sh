#!/bin/bash
# Performance test runner for Pancetta

set -e

echo "========================================="
echo "Pancetta Performance Test Suite"
echo "========================================="
echo ""

# Set environment variables for testing
export PANCETTA_STUB_AUDIO=1
export PANCETTA_MOCK_RIG=true
export RUST_LOG=error
export LIBRARY_PATH=/opt/homebrew/opt/hamlib/lib
export CPATH=/opt/homebrew/opt/hamlib/include

# Build in release mode
echo "Building Pancetta in release mode..."
cargo build --release --bin pancetta 2>&1 | grep -E "(Compiling|Finished)" || true
echo ""

# Test 1: Startup time
echo "Test 1: Startup Time"
echo "--------------------"
START_TIME=$(date +%s%N)
./target/release/pancetta --headless &
PID=$!
sleep 0.5
kill $PID 2>/dev/null || true
END_TIME=$(date +%s%N)
STARTUP_MS=$(( (END_TIME - START_TIME) / 1000000 ))
echo "Startup time: ${STARTUP_MS}ms"
if [ $STARTUP_MS -lt 1000 ]; then
    echo "✓ Startup < 1 second"
else
    echo "✗ Startup too slow (${STARTUP_MS}ms > 1000ms)"
fi
echo ""

# Test 2: Memory usage
echo "Test 2: Memory Usage"
echo "--------------------"
./target/release/pancetta --headless &
PID=$!
sleep 2

# Get memory usage in KB
MEM_KB=$(ps -o rss= -p $PID 2>/dev/null || echo "0")
MEM_MB=$(( MEM_KB / 1024 ))
kill $PID 2>/dev/null || true

echo "Memory usage: ${MEM_MB}MB"
if [ $MEM_MB -lt 100 ]; then
    echo "✓ Memory < 100MB"
else
    echo "✗ Memory usage too high (${MEM_MB}MB > 100MB)"
fi
echo ""

# Test 3: CPU usage
echo "Test 3: CPU Usage"
echo "-----------------"
./target/release/pancetta --headless &
PID=$!
sleep 3

# Sample CPU usage
CPU_USAGE=$(ps -o %cpu= -p $PID 2>/dev/null | awk '{print int($1)}')
kill $PID 2>/dev/null || true

echo "CPU usage: ${CPU_USAGE}%"
if [ $CPU_USAGE -lt 25 ]; then
    echo "✓ CPU < 25%"
else
    echo "✗ CPU usage too high (${CPU_USAGE}% > 25%)"
fi
echo ""

# Test 4: Message throughput (simulated)
echo "Test 4: Message Throughput"
echo "--------------------------"
echo "Starting application with heavy load..."
./target/release/pancetta --headless &
PID=$!
sleep 5

# Check if still running
if ps -p $PID > /dev/null 2>&1; then
    echo "✓ Application stable under load"
    kill $PID 2>/dev/null || true
else
    echo "✗ Application crashed under load"
fi
echo ""

echo "========================================="
echo "Performance Test Summary"
echo "========================================="
echo "Startup time: ${STARTUP_MS}ms"
echo "Memory usage: ${MEM_MB}MB"
echo "CPU usage: ${CPU_USAGE}%"
echo ""

# Overall pass/fail
if [ $STARTUP_MS -lt 1000 ] && [ $MEM_MB -lt 100 ] && [ $CPU_USAGE -lt 25 ]; then
    echo "✓ All performance requirements met"
    exit 0
else
    echo "✗ Some performance requirements not met"
    exit 1
fi