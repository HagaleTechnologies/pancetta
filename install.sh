#!/bin/bash

# Pancetta Installation Script
# Automatically installs Pancetta on Unix-like systems

set -e

# Colors for output
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
NC='\033[0m' # No Color

# Configuration
INSTALL_DIR="/usr/local/bin"
CONFIG_DIR="$HOME/.config/pancetta"
DATA_DIR="$HOME/.local/share/pancetta"
CACHE_DIR="$HOME/.cache/pancetta"

echo -e "${GREEN}🎚️ Pancetta Installation Script${NC}"
echo "================================="
echo ""

# Detect OS
if [[ "$OSTYPE" == "linux-gnu"* ]]; then
    OS="linux"
    ARCH=$(uname -m)
    if [[ "$ARCH" == "x86_64" ]]; then
        PLATFORM="linux-x64"
    elif [[ "$ARCH" == "aarch64" ]]; then
        PLATFORM="linux-arm64"
    elif [[ "$ARCH" == "armv7l" ]]; then
        PLATFORM="linux-armv7"
    else
        echo -e "${RED}Unsupported architecture: $ARCH${NC}"
        exit 1
    fi
elif [[ "$OSTYPE" == "darwin"* ]]; then
    OS="macos"
    ARCH=$(uname -m)
    if [[ "$ARCH" == "x86_64" ]]; then
        PLATFORM="macos-x64"
    elif [[ "$ARCH" == "arm64" ]]; then
        PLATFORM="macos-arm64"
    else
        echo -e "${RED}Unsupported architecture: $ARCH${NC}"
        exit 1
    fi
else
    echo -e "${RED}Unsupported OS: $OSTYPE${NC}"
    exit 1
fi

echo "Detected platform: $PLATFORM"
echo ""

# Check for required tools
echo "Checking prerequisites..."

# Check for audio system
if [[ "$OS" == "linux" ]]; then
    if ! command -v aplay &> /dev/null && ! command -v pactl &> /dev/null; then
        echo -e "${YELLOW}Warning: No audio system detected (ALSA or PulseAudio)${NC}"
        echo "Audio functionality may not work properly."
        read -p "Continue anyway? (y/n) " -n 1 -r
        echo
        if [[ ! $REPLY =~ ^[Yy]$ ]]; then
            exit 1
        fi
    fi
fi

# Check if already installed
if command -v pancetta &> /dev/null; then
    echo -e "${YELLOW}Pancetta is already installed${NC}"
    CURRENT_VERSION=$(pancetta --version 2>/dev/null | cut -d' ' -f2 || echo "unknown")
    echo "Current version: $CURRENT_VERSION"
    read -p "Reinstall/upgrade? (y/n) " -n 1 -r
    echo
    if [[ ! $REPLY =~ ^[Yy]$ ]]; then
        exit 0
    fi
fi

# Installation method selection
echo ""
echo "Select installation method:"
echo "1) Download pre-built binary (recommended)"
echo "2) Build from source"
echo "3) Install via Docker"
read -p "Choice (1-3): " INSTALL_METHOD

case $INSTALL_METHOD in
    1)
        # Download pre-built binary
        echo ""
        echo "Downloading Pancetta..."
        
        # Get latest release URL (replace with actual URL)
        RELEASE_URL="https://github.com/pancetta-project/pancetta/releases/latest/download"
        ARCHIVE_NAME="pancetta-latest-$PLATFORM.tar.gz"
        
        # Download
        if command -v wget &> /dev/null; then
            wget -q --show-progress "$RELEASE_URL/$ARCHIVE_NAME" -O "/tmp/$ARCHIVE_NAME" || {
                echo -e "${RED}Download failed. Building from source instead...${NC}"
                INSTALL_METHOD=2
            }
        elif command -v curl &> /dev/null; then
            curl -L --progress-bar "$RELEASE_URL/$ARCHIVE_NAME" -o "/tmp/$ARCHIVE_NAME" || {
                echo -e "${RED}Download failed. Building from source instead...${NC}"
                INSTALL_METHOD=2
            }
        else
            echo -e "${RED}Neither wget nor curl found. Building from source...${NC}"
            INSTALL_METHOD=2
        fi
        
        if [[ $INSTALL_METHOD == 1 ]]; then
            # Extract
            echo "Extracting..."
            tar -xzf "/tmp/$ARCHIVE_NAME" -C /tmp
            
            # Install binary
            echo "Installing binary..."
            sudo cp "/tmp/pancetta-$PLATFORM/pancetta" "$INSTALL_DIR/pancetta"
            sudo chmod +x "$INSTALL_DIR/pancetta"
            
            # Copy documentation
            cp -r "/tmp/pancetta-$PLATFORM/docs" "$CONFIG_DIR/" 2>/dev/null || true
            
            # Cleanup
            rm -rf "/tmp/$ARCHIVE_NAME" "/tmp/pancetta-$PLATFORM"
        fi
        ;;
        
    2)
        # Build from source
        echo ""
        echo "Building from source..."
        
        # Check for Rust
        if ! command -v cargo &> /dev/null; then
            echo "Rust not found. Installing..."
            curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
            source $HOME/.cargo/env
        fi
        
        # Clone or update repository
        if [[ -d "pancetta" ]]; then
            echo "Updating existing repository..."
            cd pancetta
            git pull
        else
            echo "Cloning repository..."
            git clone https://github.com/pancetta-project/pancetta.git
            cd pancetta
        fi
        
        # Install dependencies
        if [[ "$OS" == "linux" ]]; then
            echo "Installing build dependencies..."
            if command -v apt-get &> /dev/null; then
                sudo apt-get update
                sudo apt-get install -y pkg-config libasound2-dev libssl-dev cmake
            elif command -v dnf &> /dev/null; then
                sudo dnf install -y pkg-config alsa-lib-devel openssl-devel cmake
            elif command -v pacman &> /dev/null; then
                sudo pacman -S --noconfirm pkg-config alsa-lib openssl cmake
            fi
        elif [[ "$OS" == "macos" ]]; then
            if command -v brew &> /dev/null; then
                echo "Installing dependencies via Homebrew..."
                brew install pkg-config cmake
            fi
        fi
        
        # Build
        echo "Building Pancetta..."
        cargo build --release
        
        # Install
        echo "Installing binary..."
        sudo cp target/release/pancetta "$INSTALL_DIR/pancetta"
        
        cd ..
        ;;
        
    3)
        # Docker installation
        echo ""
        echo "Setting up Docker installation..."
        
        # Check for Docker
        if ! command -v docker &> /dev/null; then
            echo -e "${RED}Docker not found. Please install Docker first.${NC}"
            echo "Visit: https://docs.docker.com/get-docker/"
            exit 1
        fi
        
        # Create docker-compose file
        cat > docker-compose.yml << 'EOF'
version: '3.8'
services:
  pancetta:
    image: ghcr.io/pancetta-project/pancetta:latest
    container_name: pancetta
    restart: unless-stopped
    devices:
      - /dev/snd:/dev/snd
    privileged: true
    volumes:
      - ./config:/etc/pancetta
      - pancetta-data:/home/pancetta/.local/share/pancetta
    environment:
      - RUST_LOG=info
      - PANCETTA_STUB_AUDIO=false
volumes:
  pancetta-data:
EOF
        
        echo "Docker setup complete. Run with: docker-compose up -d"
        echo ""
        exit 0
        ;;
        
    *)
        echo -e "${RED}Invalid choice${NC}"
        exit 1
        ;;
esac

# Create directories
echo ""
echo "Creating directories..."
mkdir -p "$CONFIG_DIR" "$DATA_DIR" "$CACHE_DIR"

# Create default configuration if it doesn't exist
if [[ ! -f "$CONFIG_DIR/config.toml" ]]; then
    echo "Creating default configuration..."
    cat > "$CONFIG_DIR/config.toml" << 'EOF'
# Pancetta Configuration
[audio]
device_name = "default"
sample_rate = 48000
buffer_size = 512

[ft8]
decode_depth = 2
sensitivity = 0.5

[hamlib]
use_mock = true
host = "127.0.0.1"
port = 4532

[runtime]
worker_threads = 2

[qso]
my_callsign = "MYCALL"
my_grid = "EM00aa"
EOF
    echo -e "${YELLOW}Please edit $CONFIG_DIR/config.toml with your callsign and grid${NC}"
fi

# Create desktop entry (Linux only)
if [[ "$OS" == "linux" ]] && [[ -d "$HOME/.local/share/applications" ]]; then
    echo "Creating desktop entry..."
    cat > "$HOME/.local/share/applications/pancetta.desktop" << EOF
[Desktop Entry]
Name=Pancetta
Comment=FT8 Amateur Radio Application
Exec=pancetta
Icon=pancetta
Terminal=true
Type=Application
Categories=Network;HamRadio;
EOF
fi

# Verify installation
echo ""
echo "Verifying installation..."
if pancetta --version &> /dev/null; then
    VERSION=$(pancetta --version | cut -d' ' -f2)
    echo -e "${GREEN}✅ Pancetta v$VERSION installed successfully!${NC}"
else
    echo -e "${RED}❌ Installation verification failed${NC}"
    exit 1
fi

# Post-installation instructions
echo ""
echo "Installation complete!"
echo ""
echo "Next steps:"
echo "1. Edit your configuration: $CONFIG_DIR/config.toml"
echo "2. Set your callsign and grid locator"
echo "3. Configure your audio device if needed"
echo "4. Run 'pancetta' to start the application"
echo ""
echo "For help, see:"
echo "- User Guide: pancetta --help"
echo "- Documentation: $CONFIG_DIR/docs/"
echo "- GitHub: https://github.com/pancetta-project/pancetta"
echo ""
echo "73 and enjoy Pancetta!"