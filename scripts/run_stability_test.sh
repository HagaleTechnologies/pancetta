#!/bin/bash
# Long-running stability test for Pancetta

set -e

echo "==========================================="
echo "Pancetta Stability Test"
echo "==========================================="
echo ""

# Configuration
TEST_DURATION=${1:-3600}  # Default 1 hour (3600 seconds)
SAMPLE_INTERVAL=60  # Sample metrics every 60 seconds

# Set environment for testing
export PANCETTA_STUB_AUDIO=1
export PANCETTA_MOCK_RIG=true
export PANCETTA_WORKER_THREADS=2
export RUST_LOG=info
export LIBRARY_PATH=/opt/homebrew/opt/hamlib/lib
export CPATH=/opt/homebrew/opt/hamlib/include

# Build in release mode
echo "Building Pancetta..."
cargo build --release --bin pancetta 2>&1 | grep -E "(Compiling|Finished)" || true
echo ""

# Start time
START_TIME=$(date +%s)
END_TIME=$((START_TIME + TEST_DURATION))

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
NC='\033[0m'

# Metrics tracking
MAX_MEMORY=0
MAX_CPU=0
CRASH_COUNT=0
RESTART_COUNT=0
SAMPLES=0
TOTAL_CPU=0
TOTAL_MEM=0

# Function to get process metrics
get_metrics() {
    local pid=$1
    if ps -p $pid > /dev/null 2>&1; then
        CPU=$(ps -o %cpu= -p $pid | awk '{print $1}')
        MEM_KB=$(ps -o rss= -p $pid)
        MEM_MB=$((MEM_KB / 1024))
        echo "$CPU $MEM_MB"
    else
        echo "0 0"
    fi
}

# Function to start Pancetta
start_pancetta() {
    ./target/release/pancetta --headless > /tmp/pancetta_stability.log 2>&1 &
    echo $!
}

echo "Starting stability test for $TEST_DURATION seconds..."
echo "Test will end at: $(date -d @$END_TIME 2>/dev/null || date -r $END_TIME)"
echo ""
echo "Time     | CPU% | Mem MB | Status"
echo "---------|------|--------|------------------"

# Start Pancetta
PID=$(start_pancetta)
sleep 2

# Main test loop
while [ $(date +%s) -lt $END_TIME ]; do
    CURRENT_TIME=$(date +%s)
    ELAPSED=$((CURRENT_TIME - START_TIME))
    
    # Check if process is still running
    if ! ps -p $PID > /dev/null 2>&1; then
        CRASH_COUNT=$((CRASH_COUNT + 1))
        RESTART_COUNT=$((RESTART_COUNT + 1))
        
        echo -e "$(date +%H:%M:%S) | ${RED}CRASH${NC} | Process crashed (count: $CRASH_COUNT)"
        
        # Restart the process
        sleep 2
        PID=$(start_pancetta)
        echo -e "$(date +%H:%M:%S) | ${YELLOW}RESTART${NC} | Process restarted (PID: $PID)"
        sleep 3
        continue
    fi
    
    # Get metrics
    METRICS=$(get_metrics $PID)
    CPU=$(echo $METRICS | awk '{print $1}')
    MEM=$(echo $METRICS | awk '{print $2}')
    
    # Update statistics
    SAMPLES=$((SAMPLES + 1))
    TOTAL_CPU=$(echo "$TOTAL_CPU + $CPU" | bc)
    TOTAL_MEM=$((TOTAL_MEM + MEM))
    
    # Track maximums
    if (( $(echo "$CPU > $MAX_CPU" | bc -l) )); then
        MAX_CPU=$CPU
    fi
    if [ $MEM -gt $MAX_MEMORY ]; then
        MAX_MEMORY=$MEM
    fi
    
    # Display current metrics
    if [ $((ELAPSED % SAMPLE_INTERVAL)) -eq 0 ] || [ $ELAPSED -eq 0 ]; then
        STATUS="${GREEN}OK${NC}"
        if (( $(echo "$CPU > 50" | bc -l) )); then
            STATUS="${YELLOW}HIGH CPU${NC}"
        fi
        if [ $MEM -gt 200 ]; then
            STATUS="${YELLOW}HIGH MEM${NC}"
        fi
        
        printf "%s | %5.1f | %6d | %b\n" "$(date +%H:%M:%S)" "$CPU" "$MEM" "$STATUS"
    fi
    
    # Sleep before next sample
    sleep 10
done

# Stop the process
echo ""
echo "Stopping Pancetta..."
kill $PID 2>/dev/null || true
sleep 2

# Calculate averages
AVG_CPU=$(echo "scale=1; $TOTAL_CPU / $SAMPLES" | bc)
AVG_MEM=$((TOTAL_MEM / SAMPLES))

# Final report
echo ""
echo "==========================================="
echo "Stability Test Results"
echo "==========================================="
echo "Test Duration: $TEST_DURATION seconds"
echo "Total Samples: $SAMPLES"
echo ""
echo "Performance Metrics:"
echo "  Average CPU: ${AVG_CPU}%"
echo "  Maximum CPU: ${MAX_CPU}%"
echo "  Average Memory: ${AVG_MEM} MB"
echo "  Maximum Memory: ${MAX_MEMORY} MB"
echo ""
echo "Stability Metrics:"
echo "  Crashes: $CRASH_COUNT"
echo "  Restarts: $RESTART_COUNT"
echo ""

# Determine pass/fail
if [ $CRASH_COUNT -eq 0 ] && [ $AVG_MEM -lt 100 ]; then
    echo -e "Result: ${GREEN}✓ PASSED${NC}"
    echo "The application ran stable for the entire test duration."
    exit 0
else
    echo -e "Result: ${RED}✗ FAILED${NC}"
    if [ $CRASH_COUNT -gt 0 ]; then
        echo "  - Application crashed $CRASH_COUNT times"
    fi
    if [ $AVG_MEM -ge 100 ]; then
        echo "  - Average memory usage exceeded 100MB limit"
    fi
    exit 1
fi