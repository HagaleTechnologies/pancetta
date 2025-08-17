# Troubleshooting Guide

This guide helps you resolve common issues with Pancetta.

## Quick Diagnostics

Run these commands to gather diagnostic information:

```bash
# Check version and build info
./pancetta --version

# Test with stub audio (isolates audio issues)
PANCETTA_STUB_AUDIO=1 ./pancetta --headless

# Run with debug logging
RUST_LOG=debug ./pancetta 2>&1 | tee pancetta_debug.log

# Check system resources
ps aux | grep pancetta
```

## Common Issues and Solutions

### 1. Application Won't Start

#### Symptom
```
error: could not find `pancetta` in the registry
```

#### Solution
Build the application first:
```bash
cargo build --release
./target/release/pancetta
```

---

### 2. High CPU Usage

#### Symptom
- CPU usage >50%
- System becomes sluggish

#### Solutions

1. **Reduce worker threads**:
   ```bash
   export PANCETTA_WORKER_THREADS=2
   ./pancetta
   ```

2. **Disable unnecessary features**:
   ```bash
   # Run without TUI
   ./pancetta --headless
   
   # Reduce FT8 decode depth
   ./pancetta --decode-depth 1
   ```

3. **Check for busy loops**:
   ```bash
   # Monitor with top/htop
   htop -p $(pgrep pancetta)
   ```

---

### 3. Audio Device Not Found

#### Symptom
```
Error: No audio devices found
Failed to initialize audio stream
```

#### Solutions

1. **List available devices**:
   ```bash
   RUST_LOG=debug ./pancetta --list-devices
   ```

2. **Use system default device**:
   ```bash
   ./pancetta --audio-device default
   ```

3. **Test with stub audio**:
   ```bash
   PANCETTA_STUB_AUDIO=1 ./pancetta
   ```

4. **Check permissions (Linux)**:
   ```bash
   # Add user to audio group
   sudo usermod -a -G audio $USER
   # Log out and back in
   ```

5. **Check audio system (Linux)**:
   ```bash
   # For ALSA
   aplay -l
   
   # For PulseAudio
   pactl list sources
   ```

---

### 4. Hamlib/rigctld Connection Failed

#### Symptom
```
Failed to connect to rigctld: Connection refused
```

#### Solutions

1. **Check if rigctld is running**:
   ```bash
   ps aux | grep rigctld
   ```

2. **Start rigctld**:
   ```bash
   # Example for Icom IC-7300
   rigctld -m 3073 -r /dev/ttyUSB0 -s 19200
   ```

3. **Use mock rig for testing**:
   ```bash
   PANCETTA_MOCK_RIG=true ./pancetta
   ```

4. **Check connection settings**:
   ```bash
   # Test connection
   echo "f" | nc localhost 4532
   
   # Use custom host/port
   RIGCTLD_HOST=192.168.1.100 RIGCTLD_PORT=4532 ./pancetta
   ```

---

### 5. No FT8 Decodes

#### Symptom
- Application runs but no messages decoded
- Waterfall shows signals but no decodes

#### Solutions

1. **Check audio levels**:
   ```bash
   # Monitor signal strength in debug log
   RUST_LOG=debug ./pancetta | grep "signal\|level"
   ```

2. **Adjust sensitivity**:
   ```bash
   ./pancetta --sensitivity 0.8
   ```

3. **Increase decode depth**:
   ```bash
   ./pancetta --decode-depth 3
   ```

4. **Verify frequency**:
   - 14.074 MHz (20m)
   - 7.074 MHz (40m)
   - 3.573 MHz (80m)

5. **Check time synchronization**:
   ```bash
   # FT8 requires accurate time (±2 seconds)
   timedatectl status
   ```

---

### 6. Memory Usage Growing

#### Symptom
- Memory usage increases over time
- Eventually causes system slowdown

#### Solutions

1. **Monitor memory**:
   ```bash
   ./run_stability_test.sh 3600
   ```

2. **Limit message history**:
   ```toml
   # In config.toml
   [tui]
   message_list_size = 50
   ```

3. **Restart periodically**:
   ```bash
   # Use systemd service with restart
   ```

---

### 7. Terminal UI Issues

#### Symptom
- Garbled display
- Colors not working
- UI not responsive

#### Solutions

1. **Check terminal compatibility**:
   ```bash
   echo $TERM
   # Should be xterm-256color or similar
   ```

2. **Set proper terminal**:
   ```bash
   export TERM=xterm-256color
   ./pancetta
   ```

3. **Disable colors**:
   ```bash
   ./pancetta --no-color
   ```

4. **Run headless**:
   ```bash
   ./pancetta --headless
   ```

5. **Resize terminal**:
   - Minimum: 80x24
   - Recommended: 120x40

---

### 8. Build Errors

#### Symptom
```
error: linking with `cc` failed
```

#### Solutions

1. **Install build dependencies**:
   ```bash
   # macOS
   brew install pkg-config cmake
   
   # Ubuntu/Debian
   sudo apt install build-essential pkg-config cmake
   
   # Fedora
   sudo dnf install gcc pkg-config cmake
   ```

2. **Fix Hamlib linking**:
   ```bash
   # macOS
   export LIBRARY_PATH=/opt/homebrew/opt/hamlib/lib
   export CPATH=/opt/homebrew/opt/hamlib/include
   
   # Linux
   export LIBRARY_PATH=/usr/lib/x86_64-linux-gnu
   export CPATH=/usr/include
   
   cargo build --release
   ```

3. **Build without Hamlib**:
   ```bash
   cargo build --release --no-default-features
   ```

---

### 9. Configuration Not Loading

#### Symptom
- Settings in config.toml ignored
- Environment variables not working

#### Solutions

1. **Check config location**:
   ```bash
   ls -la ~/.config/pancetta/config.toml
   ```

2. **Validate TOML syntax**:
   ```bash
   # Install toml-cli
   cargo install toml-cli
   
   # Validate
   toml-cli ~/.config/pancetta/config.toml
   ```

3. **Check environment variables**:
   ```bash
   env | grep PANCETTA
   ```

4. **Show effective config**:
   ```bash
   RUST_LOG=debug ./pancetta --show-config
   ```

---

### 10. Crashes or Panics

#### Symptom
```
thread 'main' panicked at 'assertion failed'
```

#### Solutions

1. **Get backtrace**:
   ```bash
   RUST_BACKTRACE=full ./pancetta
   ```

2. **Run with debug build**:
   ```bash
   cargo run --bin pancetta
   ```

3. **Check for resource limits**:
   ```bash
   ulimit -a
   ```

4. **Increase stack size**:
   ```bash
   ulimit -s 16384
   ./pancetta
   ```

## Performance Optimization

### Reduce Latency
```bash
# Smaller buffer size
./pancetta --buffer-size 64

# Higher priority (Linux)
sudo nice -n -20 ./pancetta
```

### Reduce CPU Usage
```bash
# Fewer threads
export PANCETTA_WORKER_THREADS=1

# Lower decode depth
./pancetta --decode-depth 1

# Disable features
./pancetta --headless --no-waterfall
```

### Reduce Memory Usage
```bash
# Limit message history
./pancetta --message-limit 20

# Disable file logging
export PANCETTA_LOG_FILE=false
```

## Debug Information Collection

When reporting issues, include:

1. **System Information**:
   ```bash
   uname -a
   rustc --version
   cargo --version
   ```

2. **Debug Log**:
   ```bash
   RUST_LOG=debug ./pancetta 2>&1 | tee debug.log
   # Run for 1 minute, then Ctrl+C
   # Attach debug.log to issue
   ```

3. **Configuration**:
   ```bash
   cat ~/.config/pancetta/config.toml
   env | grep PANCETTA
   ```

4. **Backtrace** (if crashing):
   ```bash
   RUST_BACKTRACE=full ./pancetta 2>&1 | tee crash.log
   ```

## Platform-Specific Issues

### macOS

- **Microphone Permission**: System Preferences → Security & Privacy → Microphone
- **Code Signing**: May need to allow unsigned apps in Security settings

### Linux

- **Real-time Priority**: Add to `/etc/security/limits.conf`:
  ```
  @audio - rtprio 95
  @audio - memlock unlimited
  ```

- **USB Permissions**: Add udev rule for USB sound cards:
  ```bash
  echo 'SUBSYSTEM=="usb", ATTR{idVendor}=="0d8c", MODE="0666"' | \
    sudo tee /etc/udev/rules.d/99-usb-audio.rules
  sudo udevadm control --reload-rules
  ```

### Windows

- **Windows Defender**: May need to add exception
- **WASAPI Exclusive Mode**: Can cause conflicts with other apps

## Getting Help

If these solutions don't resolve your issue:

1. **Search existing issues**: [GitHub Issues](https://github.com/yourusername/pancetta/issues)
2. **Join Discord**: [Pancetta Discord](https://discord.gg/pancetta)
3. **Create new issue** with:
   - Operating system and version
   - Pancetta version (`./pancetta --version`)
   - Complete error message
   - Debug log excerpt
   - Steps to reproduce

## Emergency Recovery

If Pancetta won't start at all:

```bash
# 1. Reset configuration
mv ~/.config/pancetta/config.toml ~/.config/pancetta/config.toml.backup

# 2. Clear cache
rm -rf ~/.cache/pancetta

# 3. Test with minimal settings
PANCETTA_STUB_AUDIO=1 PANCETTA_MOCK_RIG=true ./pancetta --headless

# 4. If still failing, rebuild
cargo clean
cargo build --release
```

---

**Remember**: Most issues can be resolved by:
1. Using stub audio for testing
2. Reducing worker threads
3. Checking system permissions
4. Reading debug logs carefully

For additional help, see the [User Guide](USER_GUIDE.md) or [Configuration Guide](CONFIG.md).