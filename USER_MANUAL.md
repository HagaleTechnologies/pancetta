# Pancetta User Manual v0.1.0

## Table of Contents

1. [Overview](#overview)
2. [Installation](#installation)
3. [Initial Setup and Configuration](#initial-setup-and-configuration)
4. [Using the Terminal User Interface (TUI)](#using-the-terminal-user-interface-tui)
5. [Keyboard Shortcuts](#keyboard-shortcuts)
6. [FT8 Operation Basics](#ft8-operation-basics)
7. [Audio Configuration](#audio-configuration)
8. [Performance Monitoring](#performance-monitoring)
9. [Troubleshooting](#troubleshooting)
10. [Advanced Configuration](#advanced-configuration)

## Overview

Pancetta is a high-performance real-time audio processing application designed specifically for amateur radio FT8 digital mode operations. Built in Rust for maximum performance and safety, Pancetta delivers sub-millisecond audio latency while providing comprehensive FT8 signal processing capabilities.

### Key Features

- **Ultra-Low Latency**: Sub-millisecond audio processing for real-time operation
- **Cross-Platform**: Native support for Linux, macOS, and Windows
- **Terminal User Interface**: Clean, efficient TUI for all operations
- **Real-Time Monitoring**: Live audio levels, waterfall display, and signal analysis
- **Multi-Band Support**: Simultaneous monitoring of multiple frequency bands
- **Performance Metrics**: Built-in latency monitoring and system performance tracking

### System Requirements

- **CPU**: Modern multi-core processor (Intel i5/AMD Ryzen 5 or equivalent)
- **Memory**: 4GB RAM minimum, 8GB recommended
- **Audio**: Compatible audio interface with low-latency drivers
- **Operating System**: 
  - Linux: Ubuntu 20.04+, Fedora 34+, Arch Linux (current)
  - macOS: 10.15+ (Catalina or newer)
  - Windows: Windows 10/11 with WASAPI drivers
- **Terminal**: Modern terminal emulator with Unicode support

## Installation

For detailed installation instructions for your platform, see [INSTALL.md](INSTALL.md).

### Quick Install (Linux/macOS with Rust)

```bash
# Install via cargo
cargo install pancetta

# Or build from source
git clone https://github.com/pancetta-team/pancetta
cd pancetta
cargo build --release
```

### Package Managers

```bash
# Homebrew (macOS/Linux)
brew install pancetta

# Arch Linux (AUR)
yay -S pancetta

# Ubuntu/Debian (via .deb package)
wget https://github.com/pancetta-team/pancetta/releases/latest/download/pancetta_amd64.deb
sudo dpkg -i pancetta_amd64.deb
```

## Initial Setup and Configuration

### First Run

When you first start Pancetta, it will create default configuration files and perform an audio system check:

```bash
pancetta
```

### Configuration Directory

Pancetta stores configuration in platform-specific directories:

- **Linux**: `~/.config/pancetta/`
- **macOS**: `~/Library/Application Support/pancetta/`
- **Windows**: `%APPDATA%\pancetta\`

### Core Configuration Files

- `config.toml` - Main application configuration
- `audio.toml` - Audio device and processing settings
- `ft8.toml` - FT8-specific parameters
- `ui.toml` - User interface preferences

### Audio Device Setup

1. **List Available Devices**:
   ```bash
   pancetta --list-audio-devices
   ```

2. **Test Audio Latency**:
   ```bash
   pancetta --test-latency
   ```

3. **Configure Audio Settings**:
   Edit `~/.config/pancetta/audio.toml`:
   ```toml
   [audio]
   input_device = "Built-in Microphone"
   output_device = "Built-in Output"
   sample_rate = 48000
   buffer_size = 64
   channels_in = 2
   channels_out = 2
   ```

### Call Sign Configuration

Set your amateur radio call sign in `config.toml`:

```toml
[operator]
call_sign = "W1ABC"
grid_square = "FN42"
power_watts = 100
```

## Using the Terminal User Interface (TUI)

### Starting the Application

```bash
# Start with default configuration
pancetta

# Start with specific configuration file
pancetta --config /path/to/custom/config.toml

# Start in debug mode
pancetta --verbose

# Start with specific audio device
pancetta --audio-device "USB Audio Interface"
```

### Main Interface Layout

```
┌─────────────────────── Pancetta v0.1.0 ─────────────────────────┐
│ Status: Running | Audio: OK | FT8: Decoding | Latency: 0.89ms   │
├─────────────────────────────────────────────────────────────────┤
│                                                                 │
│  ┌─── Waterfall ───────────────┐  ┌─── Decode Window ─────────┐ │
│  │ [Frequency spectrum display] │  │ 123456 W1ABC FN42 +10    │ │
│  │                             │  │ 123500 CQ DX K1XYZ FM18   │ │
│  │ [Signal waterfall scrolling] │  │ 123600 W2DEF FN31 W1ABC  │ │
│  │                             │  │                           │ │
│  └─────────────────────────────┘  └───────────────────────────┘ │
│                                                                 │
│  ┌─── Audio Levels ────────────┐  ┌─── System Status ─────────┐ │
│  │ Input:  ████████░░ -12 dB   │  │ CPU: 3.2%                │ │
│  │ Output: ██████░░░░ -18 dB   │  │ Memory: 45.2 MB          │ │
│  │                             │  │ Uptime: 00:15:23         │ │
│  └─────────────────────────────┘  └───────────────────────────┘ │
├─────────────────────────────────────────────────────────────────┤
│ [Tab] Panels | [F1] Help | [F10] Menu | [Ctrl+C] Quit           │
└─────────────────────────────────────────────────────────────────┘
```

### Panel Navigation

The TUI is organized into resizable panels:

1. **Waterfall Panel**: Real-time frequency spectrum display
2. **Decode Window**: Live FT8 decode results
3. **Audio Levels**: Input/output signal levels
4. **System Status**: Performance metrics and status

### Panel Management

- **Tab**: Cycle through panels
- **Shift+Tab**: Cycle backwards through panels
- **Ctrl+W**: Close current panel
- **Ctrl+N**: Create new panel
- **Arrow Keys**: Navigate within active panel
- **Ctrl+Arrow**: Resize panels

## Keyboard Shortcuts

### Global Commands

| Key | Action |
|-----|--------|
| `F1` | Show help |
| `F2` | Open configuration |
| `F3` | Audio settings |
| `F4` | FT8 settings |
| `F5` | Refresh display |
| `F10` | Main menu |
| `Ctrl+C` | Exit application |
| `Ctrl+L` | Clear screen |
| `Ctrl+R` | Restart audio engine |

### Navigation

| Key | Action |
|-----|--------|
| `Tab` | Next panel |
| `Shift+Tab` | Previous panel |
| `Arrow Keys` | Navigate within panel |
| `Home` | Go to beginning |
| `End` | Go to end |
| `Page Up/Down` | Scroll page |

### Audio Control

| Key | Action |
|-----|--------|
| `Space` | Start/stop audio |
| `M` | Mute/unmute |
| `+/-` | Adjust input gain |
| `Shift++/-` | Adjust output gain |
| `R` | Reset audio levels |

### FT8 Operations

| Key | Action |
|-----|--------|
| `D` | Toggle decoding |
| `T` | Transmit mode |
| `C` | Call CQ |
| `A` | Auto-reply mode |
| `L` | Show band activity log |
| `S` | Save current session |

### View Controls

| Key | Action |
|-----|--------|
| `Z` | Zoom waterfall |
| `Shift+Z` | Zoom out waterfall |
| `F` | Full screen panel |
| `Esc` | Exit full screen |
| `I` | Toggle info display |
| `P` | Pause/resume updates |

## FT8 Operation Basics

### Understanding FT8

FT8 (Franke-Taylor design, 8-FSK modulation) is a digital weak-signal communication protocol designed for amateur radio. Key characteristics:

- **Transmission Duration**: 12.64 seconds
- **Message Encoding**: 77-bit messages with error correction
- **Frequency Shift**: 6.25 Hz between tones
- **Bandwidth**: ~50 Hz occupied bandwidth
- **Time Synchronization**: UTC time alignment required

### Operating Procedure

1. **Monitor Band Activity**:
   - Watch the waterfall for FT8 signals
   - Signals appear as parallel horizontal lines
   - Decode window shows decoded messages

2. **Frequency Selection**:
   - Standard FT8 frequencies:
     - 40m: 7.074 MHz
     - 20m: 14.074 MHz
     - 17m: 18.100 MHz
     - 15m: 21.074 MHz
     - 10m: 28.074 MHz

3. **Making Contact**:
   - Call CQ: `CQ W1ABC FN42`
   - Answer CQ: `W1ABC K1XYZ FN31`
   - Exchange signal reports: `K1XYZ W1ABC -12`
   - Acknowledge: `W1ABC K1XYZ RR73`

### Message Types

- **CQ**: General call (`CQ W1ABC FN42`)
- **CQ DX**: DX stations only (`CQ DX W1ABC FN42`)
- **Directed Call**: Specific station (`K1XYZ W1ABC FN31`)
- **Signal Report**: Signal strength (`K1XYZ W1ABC -08`)
- **Acknowledgment**: Confirm receipt (`W1ABC K1XYZ RR73`)

### Time Synchronization

FT8 requires precise time synchronization:

1. **Check System Time**:
   ```bash
   # Linux/macOS
   timedatectl status

   # Sync with NTP
   sudo timedatectl set-ntp true
   ```

2. **Windows Time Sync**:
   - Use Windows Time service
   - Or install Dimension 4 for better accuracy

3. **Accuracy Requirement**: ±1 second UTC
4. **Recommended Tools**: NTP, GPS disciplined oscillators

## Audio Configuration

### Low-Latency Audio Setup

For optimal performance, configure your system for low-latency audio:

#### Linux (ALSA/JACK)

1. **Install JACK**:
   ```bash
   sudo apt install jackd2 qjackctl
   ```

2. **Configure JACK**:
   ```bash
   # Start JACK with low latency
   jackd -d alsa -r 48000 -p 64 -n 2
   ```

3. **Pancetta with JACK**:
   ```bash
   pancetta --audio-backend jack
   ```

#### macOS (Core Audio)

1. **Check Audio MIDI Setup**:
   - Open Audio MIDI Setup
   - Set sample rate to 48000 Hz
   - Configure buffer size (64-128 samples)

2. **USB Audio Interfaces**:
   - Install manufacturer drivers
   - Use USB 3.0 ports for best performance

#### Windows (WASAPI)

1. **Windows Audio Service**:
   - Disable audio enhancements
   - Set exclusive mode in sound properties
   - Use WASAPI drivers

2. **ASIO Drivers** (recommended):
   - Install ASIO4ALL or interface-specific drivers
   - Configure buffer size (64-128 samples)

### Audio Interface Recommendations

| Interface | Latency | Price Range | Notes |
|-----------|---------|-------------|-------|
| Built-in Audio | 5-20ms | Free | Basic operation only |
| USB Audio Class | 2-10ms | $50-200 | Good for casual use |
| Professional USB | 1-3ms | $200-500 | Recommended for serious operation |
| PCIe Audio Cards | <1ms | $300-1000 | Best performance |

### Troubleshooting Audio Issues

#### High Latency

1. **Reduce Buffer Size**: Decrease to 64 or 32 samples
2. **Check CPU Usage**: High CPU can cause audio dropouts
3. **Disable Audio Enhancements**: Turn off system audio effects
4. **Update Drivers**: Ensure latest audio drivers installed

#### Audio Dropouts

1. **Increase Buffer Size**: Try 128 or 256 samples
2. **Check USB Power**: Use powered USB hubs
3. **Disable Power Management**: Prevent USB power saving
4. **Background Processes**: Close unnecessary applications

#### No Audio Input/Output

1. **Check Permissions**: Ensure microphone access granted
2. **Device Selection**: Verify correct input/output devices
3. **Sample Rate Mismatch**: Ensure all devices use same rate
4. **Cable Connections**: Verify physical connections

## Performance Monitoring

### Real-Time Metrics

Pancetta provides comprehensive performance monitoring:

#### Audio Performance

- **Callback Latency**: Target <1ms, displayed in status bar
- **Buffer Underruns**: Should be 0 during normal operation
- **Sample Rate Accuracy**: Drift monitoring
- **Input/Output Levels**: Real-time dB measurements

#### System Performance

- **CPU Usage**: Per-core utilization display
- **Memory Usage**: RAM consumption monitoring
- **Disk I/O**: Log file and recording activity
- **Network**: If using remote rig control

#### FT8 Metrics

- **Decode Rate**: Messages decoded per minute
- **Signal Quality**: SNR measurements
- **Time Synchronization**: UTC offset monitoring
- **Frequency Accuracy**: Carrier frequency tracking

### Performance Dashboard

Access detailed metrics via `Ctrl+P`:

```
┌──────────── Performance Dashboard ────────────┐
│                                               │
│ Audio Engine:                                 │
│   Latency: 0.89ms (Target: <1.0ms) ✓         │
│   Dropouts: 0 (Last hour)                    │
│   Sample Rate: 48000.1 Hz (±0.002%)          │
│                                               │
│ System Resources:                             │
│   CPU: 3.2% (Real-time thread: 1.1%)         │
│   Memory: 45.2 MB / 8.0 GB                   │
│   Disk: 0.1 MB/s write                       │
│                                               │
│ FT8 Processing:                               │
│   Decodes/min: 12                            │
│   Average SNR: -18 dB                        │
│   Time sync: +0.12s                          │
│                                               │
│ [Space] Pause | [R] Reset | [Esc] Close      │
└───────────────────────────────────────────────┘
```

### Log Files

Pancetta creates detailed log files for troubleshooting:

- **Application Log**: `~/.config/pancetta/logs/pancetta.log`
- **Audio Engine Log**: `~/.config/pancetta/logs/audio.log`
- **FT8 Decode Log**: `~/.config/pancetta/logs/ft8.log`
- **Performance Log**: `~/.config/pancetta/logs/performance.log`

## Troubleshooting

### Common Issues and Solutions

#### Application Won't Start

**Problem**: Pancetta fails to launch or crashes immediately

**Solutions**:
1. **Check Audio Permissions**:
   ```bash
   # macOS: Grant microphone access in System Preferences
   # Linux: Check user groups
   groups $USER  # Should include 'audio'
   ```

2. **Verify Dependencies**:
   ```bash
   # Check required libraries
   ldd $(which pancetta)  # Linux
   otool -L $(which pancetta)  # macOS
   ```

3. **Reset Configuration**:
   ```bash
   # Remove configuration files
   rm -rf ~/.config/pancetta
   pancetta  # Will recreate defaults
   ```

#### High Audio Latency

**Problem**: Audio latency exceeds 1ms consistently

**Solutions**:
1. **Reduce Buffer Size**:
   Edit `~/.config/pancetta/audio.toml`:
   ```toml
   [audio]
   buffer_size = 32  # Try 32, then 64
   ```

2. **Check System Load**:
   ```bash
   top  # Check for high CPU usage
   ```

3. **Disable Audio Enhancements**:
   - Windows: Sound Properties → Enhancements → Disable all
   - macOS: Audio MIDI Setup → Disable internal effects

#### FT8 Decoding Issues

**Problem**: No FT8 signals decoded or poor decode rate

**Solutions**:
1. **Time Synchronization**:
   ```bash
   # Check time accuracy
   ntpdate -q pool.ntp.org
   ```

2. **Frequency Calibration**:
   - Use WWV or other time standard
   - Check radio frequency accuracy
   - Calibrate in Pancetta settings

3. **Audio Levels**:
   - Input level: -10 to -20 dB
   - Avoid overdriving input
   - Check for clipping indicators

#### Poor Performance

**Problem**: High CPU usage or system slowdown

**Solutions**:
1. **Optimize Process Priority**:
   ```bash
   # Linux: Run with real-time priority
   sudo chrt -f 99 pancetta
   ```

2. **Close Background Applications**:
   - Web browsers with many tabs
   - Media players
   - System monitoring tools

3. **Hardware Acceleration**:
   - Enable GPU processing if available
   - Use hardware-accelerated FFT libraries

#### Configuration Issues

**Problem**: Settings not saved or incorrect behavior

**Solutions**:
1. **Check File Permissions**:
   ```bash
   ls -la ~/.config/pancetta/
   # All files should be writable by user
   ```

2. **Validate Configuration**:
   ```bash
   pancetta --validate-config
   ```

3. **Reset to Defaults**:
   ```bash
   pancetta --reset-config
   ```

### Getting Help

#### Built-in Help

- Press `F1` in any screen for context-sensitive help
- Use `pancetta --help` for command-line options
- Access full manual with `pancetta --manual`

#### Debug Mode

Run Pancetta with verbose logging:

```bash
# Enable debug output
RUST_LOG=debug pancetta

# Save debug output to file
RUST_LOG=debug pancetta 2>&1 | tee debug.log
```

#### Community Support

- **GitHub Issues**: https://github.com/pancetta-team/pancetta/issues
- **Discussion Forum**: https://github.com/pancetta-team/pancetta/discussions
- **Matrix Chat**: #pancetta:matrix.org
- **Amateur Radio Forums**: QRZ.com, Reddit r/amateurradio

### Performance Optimization

#### System Tuning

1. **Linux Real-Time Kernel**:
   ```bash
   # Install RT kernel
   sudo apt install linux-lowlatency
   ```

2. **CPU Governor**:
   ```bash
   # Set performance governor
   sudo cpupower frequency-set -g performance
   ```

3. **Memory Locking**:
   Add to `/etc/security/limits.conf`:
   ```
   username hard memlock unlimited
   username soft memlock unlimited
   ```

#### Application Tuning

1. **Process Priority**:
   ```bash
   # Linux
   sudo nice -n -20 pancetta

   # Windows
   # Set priority to "High" in Task Manager
   ```

2. **CPU Affinity**:
   ```bash
   # Bind to specific CPU cores
   taskset -c 0,1 pancetta
   ```

3. **Memory Allocation**:
   ```bash
   # Pre-allocate memory
   export PANCETTA_PREALLOC=256M
   pancetta
   ```

## Advanced Configuration

### Configuration File Reference

#### Main Configuration (`config.toml`)

```toml
[application]
# Application metadata
name = "Pancetta"
version = "0.1.0"
log_level = "info"
data_directory = "~/.local/share/pancetta"

[operator]
# Amateur radio operator information
call_sign = "W1ABC"
grid_square = "FN42"
power_watts = 100
qth = "Boston, MA"

[ui]
# User interface preferences
theme = "dark"
update_rate_ms = 50
panel_layout = "default"
show_performance_metrics = true

[logging]
# Logging configuration
enabled = true
level = "info"
file_rotation = "daily"
max_files = 30
```

#### Audio Configuration (`audio.toml`)

```toml
[audio]
# Device selection
input_device = "default"
output_device = "default"
sample_rate = 48000
buffer_size = 64
channels_in = 2
channels_out = 2

# Processing parameters
input_gain_db = 0.0
output_gain_db = 0.0
highpass_cutoff = 200.0
lowpass_cutoff = 3000.0

# Real-time parameters
thread_priority = "high"
enable_exclusive_mode = true
```

#### FT8 Configuration (`ft8.toml`)

```toml
[ft8]
# Basic parameters
center_frequency = 1500.0
decode_depth = 3
enable_auto_decode = true
enable_auto_reply = false

# Advanced parameters
sync_tolerance_ms = 500
frequency_tolerance_hz = 10
snr_threshold_db = -25
enable_deep_search = true

# Logging
log_all_decodes = true
log_directory = "~/.local/share/pancetta/logs"
```

### Plugin System

Pancetta supports plugins for extending functionality:

#### Installing Plugins

```bash
# Install from crates.io
pancetta plugin install pancetta-contest

# Install from local path
pancetta plugin install --path ./custom-plugin

# List installed plugins
pancetta plugin list
```

#### Available Plugins

- **pancetta-contest**: Contest logging and scoring
- **pancetta-digital**: Additional digital modes (PSK31, RTTY)
- **pancetta-logging**: Advanced logging and QSL management
- **pancetta-cluster**: DX cluster integration
- **pancetta-rig**: CAT control for transceivers

#### Creating Custom Plugins

See the [Plugin Development Guide](docs/PLUGIN_DEVELOPMENT.md) for details on creating custom functionality.

### Remote Operation

Pancetta supports remote operation over network connections:

#### Remote Audio

```toml
[remote_audio]
enabled = true
server_address = "192.168.1.100:8080"
compression = "opus"
latency_target_ms = 50
```

#### Remote Control

```toml
[remote_control]
enabled = true
bind_address = "0.0.0.0:8081"
api_key = "your-secret-api-key"
allowed_clients = ["192.168.1.0/24"]
```

#### Web Interface

Access Pancetta via web browser:

```bash
# Enable web interface
pancetta --web-interface --port 8080

# Open browser
open http://localhost:8080
```

---

*This manual covers Pancetta v0.1.0. For the latest documentation, visit https://github.com/pancetta-team/pancetta/docs*