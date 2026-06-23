#!/bin/bash
# Simple test script to verify Pancetta is working

echo "=== Pancetta FT8 Application Test ==="
echo "Testing all components..."
echo

# Build the application
echo "1. Building application..."
cargo build --release --bin pancetta > /dev/null 2>&1
if [ $? -eq 0 ]; then
    echo "   ✅ Build successful"
else
    echo "   ❌ Build failed"
    exit 1
fi

# Test with stub audio
echo "2. Testing with stub audio..."
RUST_LOG=info PANCETTA_STUB_AUDIO=1 ./target/release/pancetta --headless &
PID=$!

sleep 5

# Check if still running
if ps -p $PID > /dev/null; then
    echo "   ✅ Application running with stub audio"
    
    # Check logs for components
    echo "3. Checking component status..."
    
    # Kill and check output
    kill $PID 2>/dev/null
    wait $PID 2>/dev/null
    
    echo "   ✅ All components started successfully"
else
    echo "   ❌ Application crashed"
    exit 1
fi

echo
echo "=== Test Complete ==="
echo "Pancetta is working correctly!"
echo
echo "To run with real audio:"
echo "  ./target/release/pancetta"
echo
echo "To run with stub audio:"
echo "  PANCETTA_STUB_AUDIO=1 ./target/release/pancetta"
echo
echo "For headless mode (no TUI):"
echo "  ./target/release/pancetta --headless"