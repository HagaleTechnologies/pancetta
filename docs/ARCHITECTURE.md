# Pancetta System Architecture

## Executive Summary

Pancetta is a modern ham radio digital mode terminal designed with a clean, layered architecture that enables cross-platform support, comprehensive testing, and future UI flexibility. The system follows Domain-Driven Design principles with clear separation between business logic, infrastructure, and presentation layers.

## Architecture Overview

### Design Philosophy

1. **Hexagonal Architecture** - Core business logic isolated from external dependencies
2. **Domain-Driven Design** - Rich domain models representing ham radio concepts
3. **Event-Driven Communication** - Loosely coupled components via event bus
4. **Dependency Injection** - Testable, modular design with interface-based dependencies
5. **CQRS Pattern** - Separate read and write operations for clarity

### High-Level Architecture

```
┌──────────────────────────────────────────────────────────────┐
│                     Presentation Layer                        │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌──────────┐   │
│  │   TUI    │  │   CLI    │  │  Web UI  │  │  Mobile  │   │
│  │ (Ratatui)│  │  (Clap)  │  │ (Future) │  │ (Future) │   │
│  └─────┬────┘  └─────┬────┘  └─────┬────┘  └─────┬────┘   │
│        └──────────────┴──────────────┴──────────────┘        │
│                             │                                 │
│                    ┌────────▼────────┐                       │
│                    │   REST/WS API   │                       │
│                    └────────┬────────┘                       │
└────────────────────────────┼─────────────────────────────────┘
                             │
┌────────────────────────────▼─────────────────────────────────┐
│                    Application Layer                          │
│  ┌─────────────┐  ┌──────────────┐  ┌──────────────┐       │
│  │  Commands   │  │   Queries    │  │  Event Bus   │       │
│  │  Handlers   │  │   Handlers   │  │              │       │
│  └──────┬──────┘  └──────┬───────┘  └──────┬───────┘       │
│         └─────────────────┴──────────────────┘               │
│                            │                                  │
└────────────────────────────┼─────────────────────────────────┘
                             │
┌────────────────────────────▼─────────────────────────────────┐
│                      Core Domain Layer                        │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │ Digital Mode │  │  DX Hunter   │  │     QSO      │      │
│  │    Engine    │  │    Engine    │  │   Manager    │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │   Station    │  │   Contact    │  │   Logging    │      │
│  │   Manager    │  │   Database   │  │   Service    │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
└────────────────────────────────────────────────────────────┘
                             │
┌────────────────────────────▼─────────────────────────────────┐
│                   Infrastructure Layer                        │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │    Audio     │  │  Rig Control │  │   Network    │      │
│  │   Service    │  │   (Hamlib)   │  │   Services   │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐      │
│  │  Persistence │  │    File      │  │   External   │      │
│  │   (SQLite)   │  │    System    │  │     APIs     │      │
│  └──────────────┘  └──────────────┘  └──────────────┘      │
└───────────────────────────────────────────────────────────────┘
```

## Layer Descriptions

### Core Domain Layer

The heart of Pancetta containing all business logic, isolated from external dependencies.

#### Components

1. **Digital Mode Engine**
   - FT8/FT4 codec implementation
   - Message encoding/decoding
   - Protocol state machines
   - Extensible codec interface for future modes

2. **DX Hunter Engine**
   - DXCC entity management
   - Rarity scoring algorithms
   - Priority queue management
   - Contact history analysis

3. **QSO Manager**
   - QSO state machine implementation
   - Message sequencing logic
   - Automatic response generation
   - Manual override handling

4. **Station Manager**
   - Station configuration
   - Capabilities management
   - Band plan enforcement
   - Power and mode settings

5. **Contact Database**
   - In-memory contact cache
   - Query optimization
   - Statistics generation
   - Export/import logic

6. **Logging Service**
   - ADIF format handling
   - QSO validation
   - Log entry enrichment
   - Backup management

#### Domain Models

```rust
// Example domain models (language-agnostic representation)
struct Station {
    callsign: Callsign,
    grid_square: GridSquare,
    power_level: Power,
    capabilities: RigCapabilities,
}

struct QSO {
    id: QsoId,
    local_station: Station,
    remote_station: Station,
    mode: DigitalMode,
    frequency: Frequency,
    start_time: DateTime,
    end_time: Option<DateTime>,
    exchanges: Vec<Message>,
    state: QsoState,
}

struct DXEntity {
    prefix: String,
    country: String,
    continent: Continent,
    cq_zone: u8,
    itu_zone: u8,
    latitude: f64,
    longitude: f64,
}
```

### Application Layer

Orchestrates use cases and coordinates between domain and infrastructure layers.

#### Components

1. **Command Handlers**
   - StartQso, SendMessage, AbortQso
   - SetFrequency, SetMode, SetPower
   - ConfigureStation, SaveSettings

2. **Query Handlers**
   - GetHeardStations, GetQsoHistory
   - GetDXStatistics, GetBandActivity
   - GetConfiguration, GetRigStatus

3. **Event Bus**
   - MessageDecoded, QsoStarted, QsoCompleted
   - StationHeard, DXSpotted, ConfigChanged
   - RigStatusChanged, AudioLevelChanged

### Infrastructure Layer

Handles all external dependencies and I/O operations.

#### Components

1. **Audio Service**
   - Cross-platform audio abstraction (cpal/PortAudio)
   - Sample rate conversion
   - Level monitoring and AGC
   - Device enumeration and selection

2. **Rig Control**
   - Hamlib integration wrapper
   - CAT command abstraction
   - PTT control (multiple methods)
   - Frequency/mode synchronization

3. **Network Services**
   - PSKReporter client
   - QRZ.com API client (optional)
   - Time synchronization check
   - Future cloud services

4. **Persistence**
   - SQLite for contact database
   - Configuration file management
   - ADIF file I/O
   - Backup/restore operations

### Presentation Layer

Multiple UI implementations sharing the same application layer.

#### Initial TUI Implementation

```
┌─────────────────────────────────────────────────────────────┐
│                    Pancetta v1.0.0                          │
├─────────────────────────────────────────────────────────────┤
│ Band Activity                  │ QSO Status                 │
│ ┌────────────────────────────┐ │ ┌─────────────────────────┤
│ │Time  SNR  Δt   Freq  Call  │ │ │State: CALLING           │
│ │0845  -12  0.2  1245  W1AW  │ │ │Remote: EA8/G0KTN       │
│ │0845  +03  0.1  1456  JA1ABC│ │ │Sent: -06               │
│ │0845  -18  0.3  1789  VK2DEF│ │ │Rcvd: R-08              │
│ └────────────────────────────┘ │ └─────────────────────────┤
├─────────────────────────────────┴─────────────────────────────┤
│ DX Hunter                      │ Station Info               │
│ ┌────────────────────────────┐ │ ┌─────────────────────────┤
│ │Score Call      Grid   Dist │ │ │14.074.000 USB  FT8     │
│ │ 95  ZD8W      II22    8453 │ │ │TX: IDLE  RX: ACTIVE    │
│ │ 89  3B9FR     MH10   11234 │ │ │Audio: 65% █████░░░░    │
│ │ 78  VP8LP     GD18   13567 │ │ │Time: ✓ +0.1s           │
│ └────────────────────────────┘ │ └─────────────────────────┤
├─────────────────────────────────────────────────────────────┤
│ [F1]Band [F2]Mode [F3]QSO [F4]Config [F5]Log [ESC]Menu      │
└─────────────────────────────────────────────────────────────┘
```

## Data Flow

### Receive Path

```
Audio Input → Audio Service → Digital Mode Engine → Message Decoder
    ↓              ↓                   ↓                  ↓
Device Buffer → Samples → FT8 Decoder → Decoded Message → Event Bus
                                                              ↓
                                        ┌─────────────────────┴────┐
                                        ↓                          ↓
                                  QSO Manager              DX Hunter Engine
                                        ↓                          ↓
                                  State Update              Priority Update
                                        ↓                          ↓
                                    UI Update              UI Notification
```

### Transmit Path

```
User Command → Command Handler → QSO Manager → Message Generator
      ↓              ↓                ↓              ↓
 UI Input → Validate → State Check → Create Message → Encode
                                                         ↓
                                              Digital Mode Engine
                                                         ↓
                                                 FT8 Encoder
                                                         ↓
                                              Audio Service → PTT Control
                                                         ↓         ↓
                                                 Audio Output → Transceiver
```

## Interface Definitions

### Core Interfaces

```rust
// Digital Mode Codec Interface
trait DigitalModeCodec {
    fn decode(&self, samples: &[f32]) -> Result<Vec<DecodedMessage>>;
    fn encode(&self, message: &Message) -> Result<Vec<f32>>;
    fn get_mode_info(&self) -> ModeInfo;
}

// Repository Interfaces
trait ContactRepository {
    async fn save(&self, contact: Contact) -> Result<ContactId>;
    async fn find_by_callsign(&self, call: &Callsign) -> Result<Option<Contact>>;
    async fn get_worked_grids(&self) -> Result<Vec<GridSquare>>;
}

// External Service Interfaces
trait RigControl {
    async fn get_frequency(&self) -> Result<Frequency>;
    async fn set_frequency(&self, freq: Frequency) -> Result<()>;
    async fn set_ptt(&self, state: PttState) -> Result<()>;
}

trait AudioDevice {
    fn start_capture(&self, callback: AudioCallback) -> Result<()>;
    fn start_playback(&self, samples: &[f32]) -> Result<()>;
    fn get_levels(&self) -> AudioLevels;
}
```

## Concurrency Model

### Thread Architecture

1. **Main Thread** - UI event loop and user interaction
2. **Audio Thread** - Real-time audio processing (high priority)
3. **Decode Thread Pool** - Parallel FT8 decoding (CPU count - 1)
4. **Network Thread** - Async I/O for reporting and APIs
5. **Database Thread** - Background persistence operations

### Synchronization

- **Message Passing** - Channels for thread communication
- **Lock-Free Queues** - Audio buffers and decode queues
- **Event Bus** - Async event distribution
- **State Machines** - Atomic state transitions

## Error Handling Strategy

### Error Categories

1. **Recoverable Errors** - Logged, user notified, operation continues
2. **Critical Errors** - Graceful shutdown with state preservation
3. **Fatal Errors** - Emergency shutdown with crash report

### Error Propagation

```rust
// Using Result types throughout
type PancettaResult<T> = Result<T, PancettaError>;

enum PancettaError {
    Audio(AudioError),
    Codec(CodecError),
    Rig(RigError),
    Network(NetworkError),
    Database(DatabaseError),
    Configuration(ConfigError),
}
```

## Configuration Management

### Configuration Hierarchy

1. **Default Configuration** - Built-in defaults
2. **System Configuration** - `/etc/pancetta/config.toml`
3. **User Configuration** - `~/.config/pancetta/config.toml`
4. **Environment Variables** - `PANCETTA_*` overrides
5. **Command Line Arguments** - Highest priority

### Configuration Schema

```toml
[station]
callsign = "N0CALL"
grid_square = "FN31"
power = 10

[audio]
input_device = "default"
output_device = "default"
sample_rate = 48000
buffer_size = 1024

[rig]
model = "Icom IC-7300"
port = "/dev/ttyUSB0"
baud_rate = 19200
ptt_type = "CAT"

[network]
enable_pskreporter = true
enable_qrz = false
report_interval = 300

[ui]
theme = "dark"
refresh_rate = 60
show_waterfall = false
```

## Performance Considerations

### Optimization Targets

- **Audio Latency** < 50ms round-trip
- **Decode Time** < 100ms per FT8 cycle
- **UI Refresh** 60 FPS for smooth updates
- **Memory Usage** < 100MB baseline
- **CPU Usage** < 20% idle, < 80% active decode

### Optimization Strategies

1. **Zero-Copy Audio** - Direct buffer passing
2. **SIMD Decoding** - Vectorized FFT operations
3. **Memory Pools** - Pre-allocated buffers
4. **Lazy Loading** - On-demand resource loading
5. **Caching** - Computed values and API responses

## Security Architecture

### Security Principles

1. **Least Privilege** - Minimal permissions required
2. **Input Validation** - All external input sanitized
3. **Secure Storage** - Sensitive data encrypted
4. **Network Security** - TLS for external APIs
5. **Audit Logging** - Security events tracked

### Security Measures

- **Sandboxing** - Platform-specific app sandboxing
- **Code Signing** - Signed binaries for distribution
- **API Keys** - Secure storage in system keychain
- **Update Security** - Signed update packages
- **Privacy** - No telemetry without consent

## Extensibility Points

### Plugin Architecture (Future)

```rust
trait PancettaPlugin {
    fn name(&self) -> &str;
    fn version(&self) -> Version;
    fn initialize(&mut self, context: PluginContext) -> Result<()>;
    fn on_message_decoded(&self, message: &DecodedMessage);
    fn on_qso_completed(&self, qso: &QSO);
}
```

### Extension Points

1. **Custom Digital Modes** - New codec implementations
2. **UI Themes** - Custom color schemes and layouts
3. **External Services** - Additional logging/spotting services
4. **Hardware Interfaces** - Custom PTT methods
5. **Data Exporters** - Custom log formats

## Testing Architecture

### Test Pyramid

```
         ╱╲
        ╱E2E╲        5%  - End-to-end tests
       ╱──────╲
      ╱ Integr.╲     15% - Integration tests
     ╱───────────╲
    ╱   Component ╲  30% - Component tests
   ╱─────────────────╲
  ╱     Unit Tests    ╲ 50% - Unit tests
 ╱──────────────────────╲
```

### Test Infrastructure

- **Test Doubles** - Mocks for external dependencies
- **Test Fixtures** - Sample audio files and messages
- **Property Testing** - Fuzzing for codec robustness
- **Benchmark Suite** - Performance regression detection
- **Integration Harness** - Hardware-in-loop testing

## Deployment Architecture

### Distribution Packages

1. **Native Packages**
   - `.deb` for Debian/Ubuntu
   - `.rpm` for Fedora/RHEL
   - `.pkg` for macOS
   - `.msi` for Windows

2. **Universal Packages**
   - Flatpak for Linux
   - Snap for Ubuntu
   - AppImage for portable Linux

3. **Container Images**
   - Docker for testing/CI
   - Development environments

### Update Mechanism

- **Semantic Versioning** - Clear version progression
- **Delta Updates** - Minimize download size
- **Rollback Support** - Previous version retention
- **Update Channels** - Stable, Beta, Nightly

## Monitoring and Observability

### Metrics Collection

- **Application Metrics** - Decode rate, QSO count
- **Performance Metrics** - CPU, memory, latency
- **Error Metrics** - Error rates by category
- **Usage Metrics** - Feature adoption (opt-in)

### Logging Strategy

```rust
// Structured logging with levels
log::info!("QSO started"; 
    "remote_callsign" => qso.remote.callsign,
    "frequency" => qso.frequency,
    "mode" => qso.mode
);
```

## Architecture Decision Records

Key architectural decisions are documented in separate ADRs:

- [ADR-001: Language Selection](DECISIONS/ADR-001-language.md)
- [ADR-002: UI Framework](DECISIONS/ADR-002-ui-framework.md)
- [ADR-003: Audio Library](DECISIONS/ADR-003-audio.md)
- [ADR-004: Database Choice](DECISIONS/ADR-004-database.md)
- [ADR-005: Testing Strategy](DECISIONS/ADR-005-testing.md)

## Future Architecture Evolution

### Phase 1: Core Foundation (Current)
- Terminal UI implementation
- Core digital mode support
- Basic ham radio features

### Phase 2: Enhanced Features
- Web UI via WASM
- Additional digital modes
- Advanced DX features

### Phase 3: Platform Expansion
- Mobile applications
- Cloud synchronization
- Remote operation

### Phase 4: Ecosystem
- Plugin marketplace
- Community extensions
- Hardware partnerships

## Conclusion

The Pancetta architecture provides a solid foundation for a modern ham radio application that can evolve from a terminal application to a comprehensive multi-platform solution while maintaining clean separation of concerns, testability, and performance.