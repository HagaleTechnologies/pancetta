# Installation Guide

This guide will help you install Pancetta on your system.

## Table of Contents

- [System Requirements](#system-requirements)
- [Dependencies](#dependencies)
- [Platform-Specific Instructions](#platform-specific-instructions)
  - [macOS](#macos)
  - [Linux](#linux)
  - [Windows](#windows)
- [Building from Source](#building-from-source)
- [Verification](#verification)
- [Troubleshooting](#troubleshooting)

## System Requirements

### Minimum Requirements
- **CPU**: Dual-core processor (x86_64 or ARM64)
- **RAM**: 2GB
- **Storage**: 100MB free space
- **OS**: macOS 11+, Linux (kernel 5.0+), Windows 10+
- **Audio**: Working audio input/output device

### Recommended Requirements
- **CPU**: Quad-core processor or better
- **RAM**: 4GB or more
- **Audio**: External USB sound card for better performance
- **Radio**: Hamlib-compatible transceiver (optional)

## Dependencies

### Required Dependencies

1. **Rust** (1.70 or later)
   ```bash
   # Install via rustup
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   source $HOME/.cargo/env
   ```

2. **Build Tools**
   - C compiler (gcc/clang)
   - pkg-config
   - cmake (for some dependencies)

### Optional Dependencies

1. **Hamlib** (for radio control)
2. **SQLite** (usually pre-installed)

## Platform-Specific Instructions

### macOS

```bash
# Install Xcode Command Line Tools
xcode-select --install

# Install Homebrew (if not already installed)
/bin/bash -c "$(curl -fsSL https://raw.githubusercontent.com/Homebrew/install/HEAD/install.sh)"

# Install dependencies
brew install pkg-config cmake

# Install Hamlib (optional, for radio control)
brew install hamlib

# Clone and build Pancetta
git clone https://github.com/yourusername/pancetta.git
cd pancetta
cargo build --release
```

### Linux

#### Ubuntu/Debian

```bash
# Update package list
sudo apt update

# Install build dependencies
sudo apt install -y \
    build-essential \
    pkg-config \
    libasound2-dev \
    libssl-dev \
    cmake

# Install Hamlib (optional)
sudo apt install -y libhamlib-dev

# Clone and build Pancetta
git clone https://github.com/yourusername/pancetta.git
cd pancetta
cargo build --release
```

#### Fedora/RHEL

```bash
# Install build dependencies
sudo dnf install -y \
    gcc \
    pkg-config \
    alsa-lib-devel \
    openssl-devel \
    cmake

# Install Hamlib (optional)
sudo dnf install -y hamlib-devel

# Clone and build Pancetta
git clone https://github.com/yourusername/pancetta.git
cd pancetta
cargo build --release
```

#### Arch Linux

```bash
# Install build dependencies
sudo pacman -S \
    base-devel \
    pkg-config \
    alsa-lib \
    openssl \
    cmake

# Install Hamlib (optional)
sudo pacman -S hamlib

# Clone and build Pancetta
git clone https://github.com/yourusername/pancetta.git
cd pancetta
cargo build --release
```

### Windows

#### Prerequisites

1. Install [Visual Studio 2022](https://visualstudio.microsoft.com/downloads/) with:
   - Desktop development with C++
   - Windows 10/11 SDK

2. Install [Rust](https://www.rust-lang.org/tools/install):
   - Download and run rustup-init.exe
   - Follow the installation prompts

3. Install [Git for Windows](https://git-scm.com/download/win)

#### Building

```powershell
# Clone the repository
git clone https://github.com/yourusername/pancetta.git
cd pancetta

# Build the project
cargo build --release
```

## Building from Source

### Standard Build

```bash
# Clone the repository
git clone https://github.com/yourusername/pancetta.git
cd pancetta

# Build in release mode (optimized)
cargo build --release

# The binary will be at: target/release/pancetta
```

### Build with Specific Features

```bash
# Build without Hamlib support
cargo build --release --no-default-features

# Build with all features
cargo build --release --all-features
```

### Environment Variables for Building

```bash
# For Hamlib support on macOS/Linux
export LIBRARY_PATH=/opt/homebrew/opt/hamlib/lib  # macOS
export LIBRARY_PATH=/usr/lib/x86_64-linux-gnu     # Linux
export CPATH=/opt/homebrew/opt/hamlib/include      # macOS
export CPATH=/usr/include                          # Linux

cargo build --release
```

## Verification

After installation, verify that Pancetta works correctly:

```bash
# Check version
./target/release/pancetta --version

# Run basic test
PANCETTA_STUB_AUDIO=1 ./target/release/pancetta --headless

# Run with TUI (if terminal supports it)
./target/release/pancetta

# Check for Hamlib support
./target/release/pancetta --help | grep hamlib
```

## Installation Locations

### System-wide Installation (Unix-like systems)

```bash
# Copy binary to system path
sudo cp target/release/pancetta /usr/local/bin/

# Create configuration directory
mkdir -p ~/.config/pancetta

# Copy default configuration (optional)
cp config/default.toml ~/.config/pancetta/config.toml
```

### User Installation

```bash
# Copy to user's local bin
mkdir -p ~/.local/bin
cp target/release/pancetta ~/.local/bin/

# Add to PATH in ~/.bashrc or ~/.zshrc
echo 'export PATH="$HOME/.local/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

## Troubleshooting

### Common Issues

#### 1. Hamlib Not Found

**Error**: `ld: library 'hamlib' not found`

**Solution**:
```bash
# macOS
export LIBRARY_PATH=/opt/homebrew/opt/hamlib/lib
export CPATH=/opt/homebrew/opt/hamlib/include

# Linux
export LIBRARY_PATH=/usr/lib/x86_64-linux-gnu
export CPATH=/usr/include
```

#### 2. Audio Device Not Found

**Error**: `No audio devices found`

**Solution**:
- Ensure audio drivers are installed
- Check audio permissions (Linux: add user to `audio` group)
- Try with stub audio: `PANCETTA_STUB_AUDIO=1 ./pancetta`

#### 3. High CPU Usage

**Solution**:
```bash
# Reduce worker threads
export PANCETTA_WORKER_THREADS=2
./pancetta
```

#### 4. Permission Denied (Linux)

**Solution**:
```bash
# Add user to audio group
sudo usermod -a -G audio $USER
# Log out and back in for changes to take effect
```

### Getting Help

If you encounter issues not covered here:

1. Check the [Troubleshooting Guide](TROUBLESHOOTING.md)
2. Search [existing issues](https://github.com/yourusername/pancetta/issues)
3. Join our [Discord server](https://discord.gg/pancetta)
4. Create a new issue with:
   - Your operating system and version
   - Rust version (`rustc --version`)
   - Complete error message
   - Steps to reproduce

## Next Steps

After successful installation:

1. Read the [User Guide](USER_GUIDE.md) to learn how to use Pancetta
2. Check the [Configuration Guide](CONFIG.md) to customize settings
3. Join the community to share experiences and get support

---

**Happy decoding! 73**