# Pancetta Installation Guide

## Table of Contents

1. [System Requirements](#system-requirements)
2. [Quick Install](#quick-install)
3. [Platform-Specific Installation](#platform-specific-installation)
4. [Building from Source](#building-from-source)
5. [Audio System Configuration](#audio-system-configuration)
6. [Verification](#verification)
7. [Troubleshooting](#troubleshooting)

## System Requirements

### Minimum Requirements

- **CPU**: Dual-core processor @ 2.0 GHz
- **Memory**: 4GB RAM
- **Storage**: 100MB available space
- **Audio**: Compatible audio input/output device

### Recommended Requirements

- **CPU**: Quad-core processor @ 3.0 GHz or better
- **Memory**: 8GB RAM or more
- **Storage**: 1GB available space (for logs and recordings)
- **Audio**: Low-latency audio interface with ASIO/Core Audio drivers

### Operating System Support

| Platform | Version | Status | Notes |
|----------|---------|--------|--------|
| Ubuntu | 20.04+ | ✅ Supported | Primary Linux target |
| Fedora | 34+ | ✅ Supported | RPM packages available |
| Arch Linux | Current | ✅ Supported | AUR package available |
| macOS | 10.15+ | ✅ Supported | Homebrew package available |
| Windows | 10/11 | ✅ Supported | MSI installer available |
| Raspberry Pi OS | 11+ | ⚠️ Beta | ARM64 builds available |

## Quick Install

### Package Managers

#### Homebrew (macOS/Linux)

```bash
# Add Pancetta tap
brew tap pancetta-team/pancetta

# Install latest stable version
brew install pancetta

# Install development version
brew install pancetta --HEAD
```

#### Cargo (All Platforms)

```bash
# Install from crates.io
cargo install pancetta

# Install with all features
cargo install pancetta --features "full"

# Install development version from Git
cargo install --git https://github.com/pancetta-team/pancetta pancetta
```

#### Chocolatey (Windows)

```powershell
# Install Chocolatey if not already installed
Set-ExecutionPolicy Bypass -Scope Process -Force
iex ((New-Object System.Net.WebClient).DownloadString('https://chocolatey.org/install.ps1'))

# Install Pancetta
choco install pancetta
```

### Binary Downloads

Download pre-built binaries from [GitHub Releases](https://github.com/pancetta-team/pancetta/releases):

```bash
# Linux x86_64
wget https://github.com/pancetta-team/pancetta/releases/latest/download/pancetta-linux-x86_64.tar.gz
tar -xzf pancetta-linux-x86_64.tar.gz
sudo mv pancetta /usr/local/bin/

# macOS Universal
wget https://github.com/pancetta-team/pancetta/releases/latest/download/pancetta-macos-universal.tar.gz
tar -xzf pancetta-macos-universal.tar.gz
mv pancetta /usr/local/bin/

# Windows x86_64
# Download pancetta-windows-x86_64.zip and extract to desired location
```

## Platform-Specific Installation

### Ubuntu/Debian

#### Using .deb Package (Recommended)

```bash
# Download latest .deb package
wget https://github.com/pancetta-team/pancetta/releases/latest/download/pancetta_amd64.deb

# Install package
sudo dpkg -i pancetta_amd64.deb

# Install dependencies if needed
sudo apt-get install -f
```

#### Using APT Repository

```bash
# Add Pancetta repository
curl -fsSL https://apt.pancetta.dev/key.gpg | sudo gpg --dearmor -o /usr/share/keyrings/pancetta.gpg
echo "deb [signed-by=/usr/share/keyrings/pancetta.gpg] https://apt.pancetta.dev stable main" | sudo tee /etc/apt/sources.list.d/pancetta.list

# Update package list
sudo apt update

# Install Pancetta
sudo apt install pancetta
```

#### Dependencies

```bash
# Install required dependencies
sudo apt update
sudo apt install -y \
    libasound2-dev \
    libpulse-dev \
    libjack-jackd2-dev \
    pkg-config \
    build-essential

# Optional: Install JACK for low-latency audio
sudo apt install jackd2 qjackctl
```

### Fedora/RHEL/CentOS

#### Using RPM Package

```bash
# Download latest RPM package
wget https://github.com/pancetta-team/pancetta/releases/latest/download/pancetta-x86_64.rpm

# Install package
sudo rpm -i pancetta-x86_64.rpm
```

#### Using DNF Repository

```bash
# Add Pancetta repository
sudo dnf config-manager --add-repo https://rpm.pancetta.dev/pancetta.repo

# Install Pancetta
sudo dnf install pancetta
```

#### Dependencies

```bash
# Install required dependencies
sudo dnf install -y \
    alsa-lib-devel \
    pulseaudio-libs-devel \
    jack-audio-connection-kit-devel \
    pkgconf-pkg-config \
    gcc \
    gcc-c++

# Optional: Install JACK
sudo dnf install jack-audio-connection-kit qjackctl
```

### Arch Linux

#### Using AUR

```bash
# Using yay
yay -S pancetta

# Using paru
paru -S pancetta

# Manual installation
git clone https://aur.archlinux.org/pancetta.git
cd pancetta
makepkg -si
```

#### Dependencies

```bash
# Install dependencies
sudo pacman -S \
    alsa-lib \
    libpulse \
    jack2 \
    pkg-config \
    base-devel

# Optional: Install JACK tools
sudo pacman -S qjackctl cadence
```

### macOS

#### Using Homebrew (Recommended)

```bash
# Install Homebrew if not already installed
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

# Install Pancetta
brew install pancetta-team/pancetta/pancetta
```

#### Using MacPorts

```bash
# Install MacPorts if not already installed
# Download from https://www.macports.org/install.php

# Install Pancetta
sudo port install pancetta
```

#### Manual Installation

```bash
# Download macOS binary
curl -L https://github.com/pancetta-team/pancetta/releases/latest/download/pancetta-macos-universal.tar.gz -o pancetta.tar.gz

# Extract and install
tar -xzf pancetta.tar.gz
sudo mv pancetta /usr/local/bin/

# Make executable
sudo chmod +x /usr/local/bin/pancetta
```

#### macOS-Specific Setup

1. **Grant Microphone Access**:
   - Open System Preferences → Security & Privacy → Privacy → Microphone
   - Add Terminal or your terminal application
   - Check the box to grant access

2. **Disable Gatekeeper** (if needed):
   ```bash
   sudo spctl --master-disable
   # Run Pancetta, then re-enable
   sudo spctl --master-enable
   ```

### Windows

#### Using MSI Installer (Recommended)

1. Download the latest MSI installer from [GitHub Releases](https://github.com/pancetta-team/pancetta/releases)
2. Run the installer as Administrator
3. Follow the installation wizard
4. Pancetta will be added to your PATH automatically

#### Using Chocolatey

```powershell
# Install Chocolatey (if not already installed)
Set-ExecutionPolicy Bypass -Scope Process -Force
iex ((New-Object System.Net.WebClient).DownloadString('https://chocolatey.org/install.ps1'))

# Install Pancetta
choco install pancetta
```

#### Using Scoop

```powershell
# Install Scoop (if not already installed)
iwr -useb get.scoop.sh | iex

# Add Pancetta bucket
scoop bucket add pancetta https://github.com/pancetta-team/scoop-pancetta

# Install Pancetta
scoop install pancetta
```

#### Manual Installation

1. Download `pancetta-windows-x86_64.zip` from GitHub Releases
2. Extract to a folder (e.g., `C:\Program Files\Pancetta\`)
3. Add the folder to your system PATH:
   - Open System Properties → Advanced → Environment Variables
   - Edit the PATH variable and add the Pancetta folder
   - Click OK to save

#### Windows-Specific Dependencies

1. **Visual C++ Redistributables**:
   - Download and install Microsoft Visual C++ Redistributable
   - Usually installed automatically with the MSI installer

2. **Audio Drivers**:
   - Install ASIO drivers for your audio interface
   - Or install ASIO4ALL for generic ASIO support

### Raspberry Pi

#### Prerequisites

```bash
# Update system
sudo apt update && sudo apt upgrade -y

# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

# Add ARM64 target (if on 32-bit OS)
rustup target add aarch64-unknown-linux-gnu
```

#### Installation

```bash
# Method 1: Pre-built binary (ARM64 only)
wget https://github.com/pancetta-team/pancetta/releases/latest/download/pancetta-linux-aarch64.tar.gz
tar -xzf pancetta-linux-aarch64.tar.gz
sudo mv pancetta /usr/local/bin/

# Method 2: Build from source (see Building from Source section)
```

#### Performance Optimization

```bash
# Enable real-time kernel (optional)
sudo apt install linux-raspi-realtime

# Configure audio
sudo nano /boot/config.txt
# Add: dtoverlay=hifiberry-dac (for HiFiBerry cards)

# Increase USB buffer size
echo 'vm.dirty_background_bytes = 16777216' | sudo tee -a /etc/sysctl.conf
echo 'vm.dirty_bytes = 50331648' | sudo tee -a /etc/sysctl.conf
```

## Building from Source

### Prerequisites

#### Install Rust

```bash
# Install Rust via rustup
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Restart shell or source environment
source ~/.cargo/env

# Verify installation
rustc --version
cargo --version
```

#### System Dependencies

**Ubuntu/Debian:**
```bash
sudo apt install -y \
    build-essential \
    libasound2-dev \
    libpulse-dev \
    libjack-jackd2-dev \
    pkg-config \
    cmake \
    git
```

**Fedora/RHEL:**
```bash
sudo dnf install -y \
    gcc \
    gcc-c++ \
    alsa-lib-devel \
    pulseaudio-libs-devel \
    jack-audio-connection-kit-devel \
    pkgconf-pkg-config \
    cmake \
    git
```

**macOS:**
```bash
# Install Xcode command line tools
xcode-select --install

# Using Homebrew
brew install cmake pkg-config
```

**Windows:**
```powershell
# Install Visual Studio Build Tools or Visual Studio Community
# Install Git for Windows
# Install CMake
```

### Clone and Build

```bash
# Clone the repository
git clone https://github.com/pancetta-team/pancetta.git
cd pancetta

# Build in release mode (recommended)
cargo build --release

# Install to cargo bin directory
cargo install --path .

# Or copy binary manually
sudo cp target/release/pancetta /usr/local/bin/
```

### Build Options

```bash
# Build with all features
cargo build --release --features "full"

# Build without TUI (headless mode only)
cargo build --release --no-default-features --features "audio,ft8"

# Build for specific target
cargo build --release --target x86_64-unknown-linux-musl

# Cross-compile for Raspberry Pi
cargo build --release --target aarch64-unknown-linux-gnu
```

### Development Build

```bash
# Build for development (faster compilation)
cargo build

# Run tests
cargo test

# Run with debug logging
RUST_LOG=debug cargo run

# Build documentation
cargo doc --open
```

## Audio System Configuration

### Linux Audio Setup

#### ALSA Configuration

```bash
# Check available devices
aplay -l
arecord -l

# Create ALSA configuration
cat > ~/.asoundrc << EOF
pcm.!default {
    type hw
    card 0
    device 0
}
ctl.!default {
    type hw
    card 0
}
EOF
```

#### PulseAudio Setup

```bash
# Check PulseAudio status
pulseaudio --check

# List audio devices
pactl list sources short
pactl list sinks short

# Set default devices
pactl set-default-source alsa_input.usb-device
pactl set-default-sink alsa_output.usb-device
```

#### JACK Setup

```bash
# Install JACK
sudo apt install jackd2 qjackctl

# Start JACK with low latency
jackd -d alsa -r 48000 -p 64 -n 2

# Use QjackCtl for GUI configuration
qjackctl
```

### macOS Audio Setup

#### Built-in Audio

1. Open **Audio MIDI Setup** (`/Applications/Utilities/`)
2. Select your audio device
3. Set sample rate to 48000 Hz
4. Configure buffer size (64-256 samples)

#### USB Audio Interfaces

1. Install manufacturer drivers
2. Set exclusive mode in Audio MIDI Setup
3. Disable software monitoring
4. Use USB 3.0 ports for best performance

### Windows Audio Setup

#### WASAPI Configuration

1. Right-click speaker icon → **Open Sound settings**
2. Click **Device properties**
3. Click **Additional device properties**
4. Go to **Advanced** tab
5. Select **24 bit, 48000 Hz** format
6. Check **Allow applications to take exclusive control**

#### ASIO Drivers

1. **ASIO4ALL** (Generic):
   - Download from http://asio4all.org/
   - Install and configure buffer size
   - Set sample rate to 48000 Hz

2. **Interface-Specific Drivers**:
   - Download from manufacturer website
   - Install and configure via control panel
   - Set optimal buffer size (64-128 samples)

## Verification

### Test Installation

```bash
# Check version
pancetta --version

# List available audio devices
pancetta --list-audio-devices

# Test audio latency
pancetta --test-latency

# Run built-in diagnostics
pancetta --diagnostics
```

### Expected Output

```
$ pancetta --version
Pancetta 0.1.0

$ pancetta --test-latency
🎯 Pancetta Audio Latency Test
=============================

Audio Configuration:
• Sample Rate: 48000Hz
• Buffer Size: 64 samples
• Channels: 2 in, 2 out
• Theoretical Min Latency: 1.333ms

✅ Audio system initialized successfully
Input device: Built-in Microphone
Output device: Built-in Output

Testing latency for 10 seconds...

Latency Results:
• Average: 0.89ms
• Min/Max: 0.23ms / 1.45ms
• Target achieved: ✅ YES (>95% callbacks <1ms)

✅ Audio system is ready for real-time operation
```

### Performance Validation

```bash
# Run comprehensive system test
pancetta --system-test

# Monitor performance for 60 seconds
pancetta --performance-test --duration 60

# Stress test audio engine
pancetta --stress-test
```

## Troubleshooting

### Common Installation Issues

#### Rust Installation Problems

**Problem**: `rustc` command not found
```bash
# Solution: Add Rust to PATH
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

**Problem**: Old Rust version
```bash
# Solution: Update Rust
rustup update stable
```

#### Compilation Errors

**Problem**: Missing system dependencies
```bash
# Ubuntu/Debian solution
sudo apt install build-essential pkg-config libasound2-dev

# Fedora solution
sudo dnf install gcc gcc-c++ pkgconf-pkg-config alsa-lib-devel
```

**Problem**: Link errors on Windows
- Install Visual Studio Build Tools
- Ensure MSVC toolchain is selected:
  ```powershell
  rustup default stable-x86_64-pc-windows-msvc
  ```

#### Audio System Issues

**Problem**: No audio devices detected
```bash
# Linux: Check permissions
sudo usermod -a -G audio $USER
# Log out and log back in

# macOS: Grant microphone access
# System Preferences → Security & Privacy → Privacy → Microphone

# Windows: Check device drivers
# Device Manager → Audio inputs and outputs
```

**Problem**: High latency or dropouts
```bash
# Reduce buffer size
pancetta --buffer-size 32

# Check for competing processes
ps aux | grep pulse
# Kill unnecessary audio processes

# Disable power management
sudo systemctl mask sleep.target suspend.target hibernate.target hybrid-sleep.target
```

### Getting Help

#### Debug Information

```bash
# Generate debug report
pancetta --debug-report > debug.txt

# Run with verbose logging
RUST_LOG=debug pancetta 2>&1 | tee debug.log
```

#### System Information

```bash
# Collect system information
pancetta --system-info

# Audio system diagnosis
pancetta --audio-diagnosis

# Hardware compatibility check
pancetta --hardware-check
```

#### Support Channels

- **GitHub Issues**: https://github.com/pancetta-team/pancetta/issues
- **Discussions**: https://github.com/pancetta-team/pancetta/discussions
- **Matrix Chat**: #pancetta:matrix.org
- **Email**: support@pancetta.dev

When reporting issues, please include:
1. Output of `pancetta --system-info`
2. Output of `pancetta --debug-report`
3. Relevant log files from `~/.config/pancetta/logs/`
4. Steps to reproduce the problem

---

*For additional help, see the [User Manual](USER_MANUAL.md) or visit our [documentation website](https://docs.pancetta.dev)*