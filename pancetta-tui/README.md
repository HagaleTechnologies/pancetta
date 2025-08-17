# Pancetta TUI

A modern Terminal User Interface for Pancetta Ham Radio Digital Mode Monitor built with [Ratatui](https://ratatui.rs/).

## Features

### Multi-Panel Interface
- **Band Activity**: Real-time decoded message display with scrolling
- **QSO Status**: Active QSO monitoring with SNR meters and timing
- **Station Info**: Station configuration and equipment details  
- **DX Hunter**: Prioritized DX station list with scoring

### User Interface
- **Responsive Layout**: Adapts to terminal size changes
- **Keyboard Navigation**: Full keyboard control with intuitive shortcuts
- **Color Themes**: Dark and light themes with context-aware colors
- **Real-time Updates**: Live audio monitoring and message decoding

### Modern Features
- **Performance Optimized**: Sub-millisecond UI updates
- **Memory Efficient**: Configurable message history limits
- **Accessibility**: Screen reader friendly with clear navigation
- **Cross-Platform**: Works on Linux, macOS, and Windows

## Quick Start

```bash
# Build the application
cargo build --release

# Run with default configuration
./target/release/pancetta-tui

# Run with custom audio device
./target/release/pancetta-tui --device "USB Audio Device"

# Enable debug logging
./target/release/pancetta-tui --debug
```

## Keyboard Shortcuts

### Navigation
- `Tab` / `Shift+Tab`: Switch between panels
- `1-4`: Jump to specific panel
- `↑/↓` or `j/k`: Scroll within active panel
- `Page Up/Down`: Fast scroll (10 lines)

### Controls
- `M`: Toggle audio monitoring
- `T`: Switch between light/dark themes
- `Ctrl+C`: Clear current panel messages
- `Q` or `Esc`: Quit application

### Panel-Specific
- **Band Activity**: Auto-scroll to newest messages
- **QSO Status**: Real-time SNR and timing display
- **DX Hunter**: Priority-sorted station list

## Configuration

Default config location: `~/.config/pancetta/tui.toml`

```toml
[station]
call_sign = "N0CALL"
grid_square = "FN20"
power = 5
antenna = "Dipole"
rig = "IC-7300"
default_frequency = 14.074

[ui]
theme = "Dark"
refresh_rate = 250
max_messages = 1000
show_waterfall = true
time_format = "UTC24"

[audio]
device = "default"
sample_rate = 48000
buffer_size = 1024
auto_gain = true

[decoder]
enabled_modes = ["FT8", "FT4"]
minimum_snr = -24
decode_depth = 3
```

## Architecture

### Core Components
- **`main.rs`**: Application entry point and terminal setup
- **`app.rs`**: Application state and business logic
- **`events.rs`**: Async event handling (keyboard, audio, decoder)
- **`config.rs`**: Configuration management and themes

### UI Modules
- **`ui/mod.rs`**: Main layout and panel coordination
- **`ui/band_activity.rs`**: Message display with filtering
- **`ui/qso_status.rs`**: QSO state tracking and SNR meters
- **`ui/station_info.rs`**: Station details and equipment info
- **`ui/dx_hunter.rs`**: DX priority list with DXCC logic

### Custom Widgets
- **`widgets/mod.rs`**: Waterfall, signal meters, modals, spectrum display

## Development

### Building
```bash
cargo build
cargo test
cargo clippy
```

### Testing
```bash
# Run all tests
cargo test

# Test specific module
cargo test band_activity

# Integration tests
cargo test --test integration
```

### Performance
- Target: <16ms frame times (60 FPS)
- Memory: <50MB typical usage
- CPU: <5% on modern systems

## Integration

The TUI is designed to integrate with:
- **Audio Processing Pipeline**: Real-time audio from CPAL
- **FT8 Decoder**: Message decoding from ft8-lib
- **Pancetta Core**: Shared data structures and utilities

## Contributing

1. Follow the established code style
2. Add tests for new features
3. Update documentation
4. Ensure accessibility compliance

## License

MIT OR Apache-2.0