# Pancetta Technology Stack

## Executive Summary

After comprehensive evaluation, we recommend **Rust** as the primary language for Pancetta, with a modern stack emphasizing performance, safety, and cross-platform compatibility. This decision balances the need for real-time audio processing, FFI compatibility with hamlib, and a growing ecosystem that attracts contributors.

## Language Selection: Rust

### Why Rust?

#### Performance
- **Zero-cost abstractions** - High-level code compiles to efficient machine code
- **No garbage collector** - Predictable latency for real-time audio
- **SIMD support** - Explicit vectorization for DSP operations
- **Competitive with C** - Benchmark parity for signal processing

#### Safety
- **Memory safety** - No segfaults, buffer overflows, or use-after-free
- **Thread safety** - Data races prevented at compile time
- **Error handling** - Explicit Result types, no null pointers
- **Type safety** - Strong static typing catches bugs early

#### Ecosystem
- **Growing ham radio presence** - Multiple amateur radio projects
- **Excellent FFI** - Clean C interop for hamlib integration
- **Cross-platform** - Single codebase for Linux/macOS/Windows
- **Modern tooling** - Cargo, rustfmt, clippy, rust-analyzer

#### Developer Experience
- **Great documentation** - Comprehensive standard library docs
- **Active community** - Responsive forums and Discord
- **Learning resources** - "The Rust Book" and extensive tutorials
- **IDE support** - Excellent VS Code and IntelliJ integration

### Language Comparison Matrix

| Criteria | Rust | Go | Python | TypeScript | C++20 |
|----------|------|-----|--------|------------|-------|
| Performance | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐⭐⭐ |
| Memory Safety | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐ |
| FFI Support | ⭐⭐⭐⭐⭐ | ⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐ | ⭐⭐⭐⭐⭐ |
| Cross-Platform | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ |
| Ecosystem | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ |
| Learning Curve | ⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐ |
| Testing | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐ |
| Tooling | ⭐⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐ | ⭐⭐⭐⭐⭐ | ⭐⭐⭐ |

### Rejected Alternatives

#### Go
- ❌ Garbage collector introduces latency spikes
- ❌ Limited DSP libraries for signal processing
- ✅ Excellent cross-platform support
- ✅ Simple language, easy onboarding

#### Python
- ❌ Too slow for real-time audio processing
- ❌ GIL limits true parallelism
- ✅ Huge ecosystem and community
- ✅ Rapid prototyping capability

#### TypeScript/Node.js
- ❌ Not ideal for real-time audio processing
- ❌ Complex FFI with native libraries
- ✅ Excellent for web UI (future consideration)
- ✅ Large developer pool

#### C++20
- ❌ Complex memory management
- ❌ Steep learning curve
- ✅ Maximum performance potential
- ✅ Mature ecosystem

## Core Technology Stack

### Framework and Libraries

#### Application Framework
**Tokio** - Async runtime
- Industry-standard async runtime
- Excellent performance characteristics
- Rich ecosystem of compatible libraries
- Battle-tested in production

#### TUI Framework
**Ratatui** - Terminal UI
- Modern, actively maintained fork of tui-rs
- Immediate mode rendering
- Cross-platform terminal support
- Rich widget library

#### Audio Processing
**cpal** - Cross-platform audio
- Pure Rust implementation
- Supports WASAPI, CoreAudio, ALSA
- Low-latency audio streams
- Hot-plug device support

#### DSP and Codecs
**rustfft** - FFT operations
- Pure Rust FFT implementation
- SIMD optimizations
- No external dependencies

**Custom FT8 codec**
- Port of ft8_lib to Rust
- Or FFI binding to existing C library
- Future: Pure Rust implementation

#### Database
**SQLite** via **sqlx**
- Embedded database, no server required
- Async/await support
- Compile-time SQL verification
- Migration management

#### Serialization
**serde** - Serialization framework
- De facto standard in Rust
- Support for JSON, TOML, bincode
- Derive macros for ease of use

#### CLI Parsing
**clap** - Command-line parsing
- Declarative or builder API
- Automatic help generation
- Shell completion generation
- Subcommand support

#### Logging
**tracing** - Structured logging
- Async-aware logging
- Hierarchical spans
- Multiple subscriber backends
- Performance-focused design

#### Error Handling
**anyhow** + **thiserror**
- anyhow for application errors
- thiserror for library errors
- Backtraces and context
- Error chaining

### External Dependencies

#### Hamlib Integration
**hamlib-sys** - Rust FFI bindings
- Auto-generated bindings
- Safe Rust wrapper layer
- Support for 200+ transceivers

#### Network Libraries
**reqwest** - HTTP client
- Async/await support
- TLS via native-tls or rustls
- Connection pooling
- Cookie support

**tokio-tungstenite** - WebSocket
- Async WebSocket implementation
- Client and server support
- For future web UI communication

### Development Tools

#### Build System
**Cargo** - Rust's build system
- Dependency management
- Build configuration
- Test runner
- Documentation generator

#### Code Quality
**rustfmt** - Code formatter
- Consistent code style
- Configurable rules
- Pre-commit hook integration

**clippy** - Linter
- 450+ lint rules
- Correctness, performance, style
- Pedantic mode available

#### Testing Frameworks
**Built-in test framework**
- Unit tests with `#[test]`
- Integration tests in tests/
- Documentation tests
- Benchmark tests

**proptest** - Property testing
- Generative testing
- Shrinking for minimal cases
- Regex-based string generation

**criterion** - Benchmarking
- Statistical analysis
- Regression detection
- HTML reports

#### Documentation
**rustdoc** - Documentation generator
- Extracted from source comments
- Interactive examples
- Cross-references
- Search functionality

### Platform-Specific Dependencies

#### Linux
- ALSA development libraries
- libudev (for device detection)
- GTK headers (future GUI)

#### macOS
- CoreAudio (included in OS)
- CoreFoundation (included in OS)
- Security framework (keychain)

#### Windows
- Windows SDK
- WASAPI (included in OS)
- Visual C++ redistributables

## Version Management

### Dependency Versions

```toml
[dependencies]
# Core
tokio = { version = "1.35", features = ["full"] }
async-trait = "0.1"

# UI
ratatui = "0.25"
crossterm = "0.27"

# Audio
cpal = "0.15"
rubato = "0.14"  # Sample rate conversion

# DSP
rustfft = "6.1"
num-complex = "0.4"

# Database
sqlx = { version = "0.7", features = ["sqlite", "runtime-tokio-rustls"] }

# Serialization
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
toml = "0.8"

# CLI
clap = { version = "4.4", features = ["derive"] }

# Logging
tracing = "0.1"
tracing-subscriber = "0.3"

# Error handling
anyhow = "1.0"
thiserror = "1.0"

# Network
reqwest = { version = "0.11", features = ["json"] }
tokio-tungstenite = "0.21"

# Utils
chrono = "0.4"
once_cell = "1.19"
```

### Rust Version Policy
- **MSRV** (Minimum Supported Rust Version): 1.70.0
- **Update cycle**: Every 6 months
- **Edition**: Rust 2021

## Build Configuration

### Cargo.toml Structure

```toml
[package]
name = "pancetta"
version = "1.0.0"
edition = "2021"
rust-version = "1.70"

[workspace]
members = [
    "pancetta-core",
    "pancetta-tui",
    "pancetta-audio",
    "pancetta-codecs",
]

[profile.release]
opt-level = 3
lto = true
codegen-units = 1
strip = true

[profile.release-debug]
inherits = "release"
debug = true
strip = false
```

### Feature Flags

```toml
[features]
default = ["tui", "hamlib", "pskreporter"]
tui = ["dep:ratatui", "dep:crossterm"]
hamlib = ["dep:hamlib-sys"]
pskreporter = ["dep:reqwest"]
qrz = ["dep:reqwest"]
experimental = ["unstable-codecs"]
```

## Testing Infrastructure

### Test Organization

```
tests/
├── unit/           # Unit tests (in src/ files)
├── integration/    # Integration tests
│   ├── audio.rs
│   ├── codec.rs
│   └── hamlib.rs
├── e2e/           # End-to-end tests
│   └── scenarios/
└── fixtures/      # Test data
    ├── audio/
    └── messages/
```

### Continuous Integration

#### GitHub Actions Workflow

```yaml
name: CI

on: [push, pull_request]

jobs:
  test:
    strategy:
      matrix:
        os: [ubuntu-latest, macos-latest, windows-latest]
        rust: [stable, beta]
    
    steps:
    - uses: actions/checkout@v3
    - uses: dtolnay/rust-toolchain@stable
    - uses: Swatinem/rust-cache@v2
    - run: cargo build --all-features
    - run: cargo test --all-features
    - run: cargo clippy -- -D warnings
    - run: cargo fmt -- --check
```

## Package Management

### Distribution Formats

#### Cargo (Primary)
```bash
cargo install pancetta
```

#### Platform Packages
- **Homebrew** (macOS/Linux): `brew install pancetta`
- **AUR** (Arch Linux): `yay -S pancetta`
- **Snap**: `snap install pancetta`
- **Flatpak**: `flatpak install io.github.pancetta`

#### Binary Releases
- GitHub Releases with pre-built binaries
- Checksums and signatures
- Automatic release notes

### Dependency Security

#### cargo-audit
```bash
cargo install cargo-audit
cargo audit
```

#### cargo-deny
```toml
# deny.toml
[bans]
multiple-versions = "warn"
wildcards = "deny"

[licenses]
unlicensed = "deny"
copyleft = "warn"
```

## Performance Optimization

### Compilation Optimizations

```toml
[profile.release]
opt-level = 3          # Maximum optimizations
lto = true            # Link-time optimization
codegen-units = 1     # Single codegen unit
panic = "abort"       # Smaller binary
strip = true          # Strip symbols
```

### Runtime Optimizations

- **SIMD**: Explicit SIMD for DSP operations
- **Parallelism**: Rayon for parallel decoding
- **Memory pools**: Object pooling for allocations
- **Zero-copy**: Minimize data copying

## Future Technology Considerations

### Web UI (Phase 2)
- **Frontend**: React/TypeScript or Leptos (Rust/WASM)
- **Backend**: Axum web framework
- **Protocol**: WebSocket for real-time updates
- **Deployment**: Single binary with embedded assets

### Mobile (Phase 3)
- **iOS/Android**: React Native or Flutter
- **Core**: Shared Rust core via FFI
- **Audio**: Platform-specific audio APIs
- **Distribution**: App stores

### Cloud Services (Phase 4)
- **Backend**: Rust microservices
- **Database**: PostgreSQL for multi-user
- **Queue**: Redis for job processing
- **Storage**: S3-compatible for backups

## Technology Governance

### Decision Process
1. Propose via RFC (Request for Comments)
2. Community discussion period (1 week)
3. Core team review and decision
4. Document in ADR (Architecture Decision Record)

### Upgrade Policy
- **Major dependencies**: Quarterly review
- **Security updates**: Immediate
- **Breaking changes**: Major version only
- **Deprecation**: 2 version warning period

## Risk Mitigation

### Technology Risks

| Risk | Impact | Mitigation |
|------|--------|------------|
| Rust learning curve | Medium | Comprehensive documentation, mentoring |
| Library ecosystem gaps | Low | FFI fallback, contribute upstream |
| Platform compatibility | Low | CI testing on all platforms |
| Performance regression | Medium | Automated benchmarking |
| Dependency abandonment | Low | Fork critical dependencies |

## Conclusion

The selected technology stack positions Pancetta as a modern, performant, and maintainable ham radio application. Rust provides the perfect balance of performance for real-time audio processing and safety for reliable operation. The chosen libraries are mature, well-maintained, and provide excellent cross-platform support.

This stack enables:
- ✅ Real-time audio processing with predictable latency
- ✅ Safe concurrency for multi-core utilization
- ✅ Clean FFI for hamlib integration
- ✅ Cross-platform deployment from single codebase
- ✅ Modern development experience attracting contributors
- ✅ Future expansion to web and mobile platforms

The technology choices prioritize long-term maintainability while delivering the performance required for digital signal processing in amateur radio applications.