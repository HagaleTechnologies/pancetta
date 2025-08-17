# Configuration Guide

Pancetta offers flexible configuration through multiple methods. This guide covers all available options.

## Configuration Hierarchy

Configuration sources are applied in the following order (later sources override earlier ones):

1. **Default values** (built into the application)
2. **Configuration file** (`~/.config/pancetta/config.toml`)
3. **Environment variables** (prefixed with `PANCETTA_`)
4. **Command-line arguments** (highest priority)

## Configuration File

### Location

- **Unix/Linux/macOS**: `~/.config/pancetta/config.toml`
- **Windows**: `%APPDATA%\pancetta\config.toml`

### Complete Example

```toml
# ~/.config/pancetta/config.toml

# Audio Configuration
[audio]
# Audio device name (use "default" for system default)
device_name = "default"

# Sample rate in Hz (48000 or 44100 recommended)
sample_rate = 48000

# Buffer size in samples (lower = less latency, higher = more stable)
# Powers of 2 recommended: 64, 128, 256, 512, 1024
buffer_size = 512

# Number of audio channels (1 = mono, 2 = stereo)
channels = 1

# Target latency in milliseconds
latency_ms = 10

# DSP Configuration
[dsp]
# DSP sample rate (12000 Hz for FT8)
sample_rate = 12000

# Enable noise reduction
noise_reduction = true

# Noise reduction strength (0.0 to 1.0)
noise_reduction_strength = 0.5

# Enable automatic gain control
agc_enabled = true

# AGC target level (-40 to 0 dB)
agc_target_db = -20

# Bandpass filter settings
bandpass_low_freq = 200.0
bandpass_high_freq = 3000.0

# FT8 Decoder Configuration
[ft8]
# Decode depth (1-3, higher = more CPU but better weak signal decode)
decode_depth = 3

# Sensitivity threshold (0.0 to 1.0)
sensitivity = 0.5

# Maximum simultaneous decodes
max_decodes = 50

# Enable deep search
deep_search = true

# Time window tolerance in seconds
time_tolerance = 2.0

# Frequency tolerance in Hz
frequency_tolerance = 50.0

# Hamlib Configuration
[hamlib]
# rigctld host address
host = "127.0.0.1"

# rigctld port
port = 4532

# Connection timeout in milliseconds
timeout_ms = 5000

# Retry count for failed commands
retry_count = 3

# Use mock rig for testing
use_mock = false

# QSO Logging Configuration
[qso]
# Database file path
database_path = "~/.local/share/pancetta/qsos.db"

# Enable automatic logging
auto_log = true

# Station information
my_callsign = "MYCALL"
my_grid = "EM00aa"
my_name = "Operator Name"

# ADIF export settings
adif_export_path = "~/Documents/pancetta_log.adi"

# TUI Configuration
[tui]
# Enable terminal UI
enabled = true

# Refresh rate in milliseconds
refresh_ms = 100

# Color scheme ("dark", "light", "high_contrast")
color_scheme = "dark"

# Show waterfall display
show_waterfall = true

# Waterfall height in terminal lines
waterfall_height = 10

# Message list size
message_list_size = 50

# Logging Configuration
[logging]
# Log level: "error", "warn", "info", "debug", "trace"
level = "info"

# Enable file logging
file_logging = false

# Log file directory
log_dir = "~/.cache/pancetta/logs"

# Use JSON format for logs
json_format = false

# Show thread IDs in logs
show_thread_ids = false

# Runtime Configuration
[runtime]
# Number of worker threads (0 = auto-detect)
worker_threads = 2

# Enable performance metrics
enable_metrics = true

# Metrics export port (for Prometheus)
metrics_port = 9090
```

## Environment Variables

All configuration options can be set via environment variables. Use uppercase names with underscores, prefixed with `PANCETTA_`.

### Common Environment Variables

```bash
# Logging
export RUST_LOG=debug                    # Log level (error/warn/info/debug/trace)
export PANCETTA_LOG_FILE=true           # Enable file logging

# Runtime
export PANCETTA_WORKER_THREADS=2        # Number of worker threads
export PANCETTA_ENABLE_METRICS=true     # Enable performance metrics

# Audio
export PANCETTA_AUDIO_DEVICE="hw:1,0"   # Specific audio device
export PANCETTA_STUB_AUDIO=true         # Use stub audio (testing)

# Hamlib
export PANCETTA_MOCK_RIG=false          # Use real rigctld
export RIGCTLD_HOST=192.168.1.100       # Remote rigctld host
export RIGCTLD_PORT=4532                # rigctld port

# FT8
export PANCETTA_FT8_DEPTH=3             # Decode depth
export PANCETTA_FT8_SENSITIVITY=0.7     # Sensitivity threshold
```

### Setting Environment Variables

#### Unix/Linux/macOS

```bash
# Temporary (current session only)
export PANCETTA_WORKER_THREADS=2
./pancetta

# Or inline
PANCETTA_WORKER_THREADS=2 ./pancetta

# Permanent (add to ~/.bashrc or ~/.zshrc)
echo 'export PANCETTA_WORKER_THREADS=2' >> ~/.bashrc
source ~/.bashrc
```

#### Windows

```powershell
# Temporary (current session)
$env:PANCETTA_WORKER_THREADS = "2"
.\pancetta.exe

# Permanent (user level)
[System.Environment]::SetEnvironmentVariable("PANCETTA_WORKER_THREADS", "2", "User")
```

## Command-Line Arguments

Command-line arguments override all other configuration sources.

```bash
# Basic options
pancetta --help                    # Show help message
pancetta --version                 # Show version
pancetta --config /path/to/config  # Use specific config file

# Audio options
pancetta --audio-device "hw:1,0"   # Use specific audio device
pancetta --buffer-size 256         # Set buffer size
pancetta --sample-rate 48000       # Set sample rate

# FT8 options
pancetta --decode-depth 3          # Set decode depth
pancetta --sensitivity 0.7         # Set sensitivity

# UI options
pancetta --headless                # Run without TUI
pancetta --no-waterfall           # Disable waterfall display

# Hamlib options
pancetta --rig-host 192.168.1.100  # rigctld host
pancetta --rig-port 4532           # rigctld port
pancetta --mock-rig                # Use mock rig

# Logging options
pancetta --log-level debug         # Set log level
pancetta --log-file                # Enable file logging
```

## Configuration Profiles

You can create multiple configuration profiles for different scenarios:

### Contest Configuration

```toml
# ~/.config/pancetta/contest.toml
[ft8]
decode_depth = 1          # Faster decoding
max_decodes = 100         # More simultaneous decodes
sensitivity = 0.3         # Only strong signals

[runtime]
worker_threads = 8        # Maximum performance

[logging]
level = "warn"           # Less logging overhead
```

### Weak Signal Configuration

```toml
# ~/.config/pancetta/weak_signal.toml
[ft8]
decode_depth = 3         # Maximum depth
deep_search = true       # Enable deep search
sensitivity = 0.9        # Maximum sensitivity
time_tolerance = 3.0     # Wider time window

[dsp]
noise_reduction_strength = 0.8  # Strong noise reduction
```

### Load a Profile

```bash
pancetta --config ~/.config/pancetta/contest.toml
```

## Audio Device Selection

### List Available Devices

```bash
# Pancetta will list devices on startup with debug logging
RUST_LOG=debug pancetta --headless
```

### Device Names by Platform

#### macOS
```bash
"Built-in Microphone"
"Built-in Output"
"USB Audio Device"
```

#### Linux (ALSA)
```bash
"default"                  # System default
"hw:0,0"                  # Hardware device
"plughw:1,0"              # USB device with format conversion
"pulse"                   # PulseAudio
```

#### Windows
```bash
"Microphone (Realtek Audio)"
"Speakers (Realtek Audio)"
"Line In (USB Audio Device)"
```

## Performance Tuning

### Low Latency Configuration

```toml
[audio]
buffer_size = 64          # Minimum buffer
latency_ms = 5           # Target 5ms

[runtime]
worker_threads = 2       # Fewer threads, less overhead

[tui]
refresh_ms = 200        # Slower UI updates
```

### High Reliability Configuration

```toml
[audio]
buffer_size = 1024      # Large buffer
latency_ms = 20        # Higher latency tolerance

[hamlib]
retry_count = 5        # More retries
timeout_ms = 10000     # Longer timeout
```

### Low CPU Configuration

```toml
[runtime]
worker_threads = 1      # Single worker thread

[ft8]
decode_depth = 1       # Shallow decode
max_decodes = 10       # Limit simultaneous decodes

[tui]
enabled = false        # Disable TUI
```

## Validation

Pancetta validates configuration on startup. Invalid values will produce warnings and fall back to defaults.

```bash
# Test configuration without running
pancetta --validate-config

# Show effective configuration
RUST_LOG=debug pancetta --show-config
```

## Troubleshooting Configuration

### Config File Not Loading

1. Check file location:
   ```bash
   ls -la ~/.config/pancetta/config.toml
   ```

2. Validate TOML syntax:
   ```bash
   cat ~/.config/pancetta/config.toml | python3 -m tomli
   ```

### Environment Variables Not Working

1. Check variable is exported:
   ```bash
   echo $PANCETTA_WORKER_THREADS
   ```

2. Check for typos (must be uppercase with underscores)

### Performance Issues

1. Start with default configuration
2. Change one setting at a time
3. Monitor with metrics:
   ```bash
   PANCETTA_ENABLE_METRICS=true pancetta
   ```

## Best Practices

1. **Start with defaults** - Only change what you need
2. **Use configuration file** for permanent settings
3. **Use environment variables** for temporary overrides
4. **Use command-line arguments** for one-off testing
5. **Document your changes** with comments in config file
6. **Backup your configuration** before major changes
7. **Test incrementally** when tuning performance

---

For more help, see the [Troubleshooting Guide](TROUBLESHOOTING.md) or join our community chat.