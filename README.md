# Pancetta

Autonomous FT8 ham radio station written in Rust.

## What It Does

- **FT8 decoding**: LDPC error correction, OSD (Ordered Statistics Decoding), AP decoding — bit-exact with WSJT-X
- **Autonomous QSO operation**: Hunt mode (pounce on rare stations), CQ mode (answer callers), hybrid mode
- **Priority-based station selection**: Weighted scoring — needed DXCC > needed grid > POTA/SOTA > rarity, with duplicate suppression and failure backoff
- **Multi-stream TX**: N simultaneous FT8 signals in a single 15-second slot via SmartFrequencyAllocator
- **Real-time TUI**: Terminal interface with waterfall display (scaffold, not yet wired to live pipeline)

## Workspace Structure

11-crate Cargo workspace:

| Crate | Purpose | Status |
|-------|---------|--------|
| `pancetta-ft8` | FT8 encoder/decoder/modulator/OSD | Production-grade, ~200 tests, bit-exact with ft8_lib/WSJT-X |
| `pancetta-audio` | Real-time audio I/O (cpal + ringbuf) | Functional |
| `pancetta-dsp` | DSP pipeline (FFT, filtering, resampling) | Functional |
| `pancetta-config` | Configuration with hot-reload | Production-ready, ~59 tests |
| `pancetta-qso` | QSO management, priority scoring, frequency allocation, autonomous operator | Core logic, ~81 tests |
| `pancetta-dx` | DX hunting, DXCC, PSKReporter | Partial implementation |
| `pancetta-hamlib` | Hamlib CAT control FFI | Bindings done, integration stub |
| `pancetta-cqdx` | cqdx.io HTTP client, cache, types | Delta-adapted, needs live API validation |
| `pancetta-tui` | Terminal UI | Scaffold, not wired to pipeline |
| `pancetta-core` | Shared types, error handling | Stable |
| `pancetta` | Main binary, coordinator, message bus, runtime | Integration point |

## Quick Start

Prerequisites: Rust (install from [rustup.rs](https://rustup.rs/))

```bash
# Build the workspace
cargo build

# Run all workspace tests
cargo test

# FT8 tests including encoder (feature-gated behind `transmit`)
cargo test --features transmit -p pancetta-ft8

# Loopback integration tests (end-to-end QSO through encode→modulate→decode)
cargo test -p pancetta --test loopback_qso

# Run the application
cargo run
```

Note: `pancetta-hamlib` hangs in workspace builds due to tokio runtime conflicts. Test it separately:

```bash
cargo test -p pancetta-hamlib --lib -- --test-threads=1
```

## Configuration

Config lives at `~/.pancetta/config.toml`. See `docs/CONFIG.md` for all options.

## Documentation

- `FEATURES.md` — capabilities and feature status
- `docs/ARCHITECTURE.md` — system design and internals
- `CLAUDE.md` — development instructions, known gaps, build hygiene

## License

MIT — see [LICENSE](LICENSE)
