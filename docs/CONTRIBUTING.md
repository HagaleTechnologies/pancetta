# Contributing to Pancetta

Thank you for your interest in contributing to Pancetta! This guide will help you get started with contributing to our high-performance amateur radio FT8 processing application.

## Table of Contents

1. [Code of Conduct](#code-of-conduct)
2. [Getting Started](#getting-started)
3. [Development Environment](#development-environment)
4. [Project Structure](#project-structure)
5. [Contribution Workflow](#contribution-workflow)
6. [Coding Standards](#coding-standards)
7. [Testing Guidelines](#testing-guidelines)
8. [Performance Requirements](#performance-requirements)
9. [Documentation](#documentation)
10. [Issue Guidelines](#issue-guidelines)
11. [Pull Request Process](#pull-request-process)
12. [Release Process](#release-process)

## Code of Conduct

We are committed to providing a friendly, safe, and welcoming environment for all contributors. Please read and follow our [Code of Conduct](CODE_OF_CONDUCT.md).

### Our Standards

- **Be respectful**: Treat all community members with respect and courtesy
- **Be inclusive**: Welcome newcomers and help them get started
- **Be constructive**: Provide helpful feedback and suggestions
- **Be professional**: Keep discussions focused on technical topics
- **Be patient**: Remember that everyone is learning and growing

## Getting Started

### Prerequisites

Before contributing, ensure you have:

- **Rust** 1.70.0 or later
- **Git** for version control
- **Audio hardware** for testing (USB audio interface recommended)
- **Amateur radio license** (for FT8 testing on-air)
- **Time synchronization** (NTP) for accurate FT8 timing

### Quick Setup

```bash
# Fork the repository on GitHub
# Clone your fork
git clone https://github.com/YOUR_USERNAME/pancetta.git
cd pancetta

# Add upstream remote
git remote add upstream https://github.com/pancetta-team/pancetta.git

# Install dependencies
cargo build

# Run tests
cargo test

# Run the application
cargo run
```

## Development Environment

### Recommended Tools

- **IDE**: VS Code with rust-analyzer extension
- **Terminal**: Modern terminal with Unicode support
- **Audio Tools**: 
  - Linux: JACK, PulseAudio, or ALSA tools
  - macOS: Audio MIDI Setup
  - Windows: ASIO drivers and control panel

### VS Code Extensions

```json
{
  "recommendations": [
    "rust-lang.rust-analyzer",
    "vadimcn.vscode-lldb",
    "serayuzgur.crates",
    "tamasfe.even-better-toml",
    "ms-vscode.test-adapter-converter"
  ]
}
```

### Environment Variables

```bash
# Development environment
export RUST_LOG=debug
export RUST_BACKTRACE=1
export PANCETTA_CONFIG_PATH=./dev-config

# Performance testing
export RUST_LOG=info
export PANCETTA_ENABLE_METRICS=true
```

## Project Structure

### Workspace Organization

```
pancetta/
├── pancetta/              # Main application binary
├── pancetta-audio/        # Real-time audio processing engine
├── pancetta-dsp/          # Digital signal processing primitives  
├── pancetta-ft8/          # FT8 protocol implementation
├── pancetta-tui/          # Terminal user interface
├── pancetta-config/       # Configuration management
├── docs/                  # Documentation
├── scripts/               # Build and development scripts
├── examples/              # Usage examples
└── tests/                 # Integration tests
```

### Core Crate Responsibilities

| Crate | Purpose | Performance Critical |
|-------|---------|---------------------|
| `pancetta-audio` | Real-time audio I/O, <1ms latency | ✅ **Critical** |
| `pancetta-dsp` | FFT, filtering, signal processing | ✅ **High** |
| `pancetta-ft8` | FT8 encode/decode, protocol | ⚠️ **Medium** |
| `pancetta-tui` | User interface, visualization | ⚠️ **Low** |
| `pancetta-config` | Configuration management | ⚠️ **Low** |
| `pancetta` | Orchestration, CLI | ⚠️ **Low** |

## Contribution Workflow

### 1. Choose an Issue

- Browse [open issues](https://github.com/pancetta-team/pancetta/issues)
- Look for `good-first-issue` or `help-wanted` labels
- Comment on the issue to indicate you're working on it
- For major changes, create an issue first to discuss approach

### 2. Create Feature Branch

```bash
# Fetch latest changes
git fetch upstream
git checkout main
git merge upstream/main

# Create feature branch
git checkout -b feature/your-feature-name

# Or for bug fixes
git checkout -b fix/issue-number-description
```

### 3. Make Changes

Follow our coding standards and testing guidelines (see below).

### 4. Test Your Changes

```bash
# Run all tests
cargo test

# Run integration tests
cargo test --test integration

# Run performance benchmarks
cargo bench

# Test on your platform
cargo run -- --test-latency
```

### 5. Commit Changes

```bash
# Stage changes
git add .

# Commit with descriptive message
git commit -m "feat: add waterfall frequency zoom functionality

- Implement zoom controls for waterfall display
- Add keyboard shortcuts (Z/Shift+Z)
- Update configuration schema for zoom settings
- Add tests for zoom functionality

Closes #123"
```

### 6. Push and Create PR

```bash
# Push to your fork
git push origin feature/your-feature-name

# Create PR on GitHub with detailed description
```

## Coding Standards

### Rust Style Guidelines

We follow the official Rust style guide with some project-specific conventions:

#### Code Formatting

```bash
# Format code before committing
cargo fmt

# Check formatting in CI
cargo fmt -- --check

# Lint with Clippy
cargo clippy -- -D warnings
```

#### Naming Conventions

```rust
// Use descriptive names for real-time components
struct AudioCallbackProcessor {
    pre_allocated_buffer: Vec<f32>,
    ringbuffer_producer: Producer<AudioFrame>,
    latency_monitor: LatencyTracker,
}

// Prefix error types consistently
#[derive(Debug, thiserror::Error)]
pub enum AudioError {
    #[error("Device not found: {device_name}")]
    DeviceNotFound { device_name: String },
    
    #[error("Latency target exceeded: {actual_ms}ms > {target_ms}ms")]
    LatencyExceeded { actual_ms: f64, target_ms: f64 },
}

// Use type aliases for clarity
pub type AudioSample = f32;
pub type FrequencyHz = f64;
pub type LatencyMs = f64;
```

#### Documentation Standards

```rust
/// Real-time audio processing engine with sub-millisecond latency.
///
/// This engine provides lock-free communication between the audio callback
/// thread and the main application thread. All audio processing must complete
/// within the configured latency target (typically <1ms).
///
/// # Examples
///
/// ```rust
/// use pancetta_audio::{AudioEngine, AudioConfig};
///
/// let config = AudioConfig {
///     sample_rate: 48000,
///     buffer_size: 64,
///     ..Default::default()
/// };
///
/// let engine = AudioEngine::new(config)?;
/// engine.start(callback)?;
/// ```
///
/// # Performance Requirements
///
/// - Audio callback must complete in <1ms
/// - No heap allocations in audio thread
/// - No blocking operations in audio thread
/// - Use lock-free data structures for communication
pub struct AudioEngine {
    // Implementation details...
}

impl AudioEngine {
    /// Creates a new audio engine with the specified configuration.
    ///
    /// # Arguments
    ///
    /// * `config` - Audio configuration including sample rate and buffer size
    ///
    /// # Returns
    ///
    /// Returns `Ok(AudioEngine)` on success, or `AudioError` if the configuration
    /// is invalid or audio system initialization fails.
    ///
    /// # Errors
    ///
    /// This function will return an error if:
    /// - Sample rate is not supported by the audio system
    /// - Buffer size is invalid (must be power of 2, 32-512)
    /// - Audio devices are not available
    ///
    /// # Performance Notes
    ///
    /// Smaller buffer sizes reduce latency but increase CPU usage.
    /// Recommended buffer size: 64 samples at 48kHz (1.33ms).
    pub fn new(config: AudioConfig) -> Result<Self, AudioError> {
        // Implementation...
    }
}
```

### Error Handling

```rust
// Use thiserror for structured errors
#[derive(Debug, thiserror::Error)]
pub enum Ft8Error {
    #[error("Invalid message format: {message}")]
    InvalidMessage { message: String },
    
    #[error("Decode failed: SNR {snr:.1} dB below threshold {threshold:.1} dB")]
    WeakSignal { snr: f64, threshold: f64 },
    
    #[error("Time synchronization error: offset {offset_ms:.1} ms")]
    TimingError { offset_ms: f64 },
}

// Use anyhow for application-level errors
use anyhow::{Context, Result};

pub async fn run_application() -> Result<()> {
    let config = load_config()
        .context("Failed to load configuration")?;
    
    let audio_engine = AudioEngine::new(config.audio)
        .context("Failed to initialize audio engine")?;
    
    // ... rest of application
    
    Ok(())
}
```

### Performance-Critical Code

```rust
// Real-time audio callback - NO allocations!
impl AudioCallback for MyCallback {
    fn process(&mut self, input: &[f32], output: &mut [f32], _info: &AudioCallbackInfo) -> AudioCallbackResult {
        // ✅ Good: Pre-allocated buffer
        let temp = &mut self.temp_buffer[..input.len()];
        
        // ✅ Good: Lock-free communication
        if self.ringbuffer.push_slice(input).is_err() {
            self.overflow_count.fetch_add(1, Ordering::Relaxed);
        }
        
        // ✅ Good: Bounded, predictable processing
        for (i, &sample) in input.iter().enumerate() {
            temp[i] = sample * self.gain;
        }
        
        output.copy_from_slice(temp);
        
        AudioCallbackResult::Continue
    }
}

// ❌ Bad: This will cause audio dropouts
impl AudioCallback for BadCallback {
    fn process(&mut self, input: &[f32], output: &mut [f32], _info: &AudioCallbackInfo) -> AudioCallbackResult {
        // ❌ Heap allocation
        let mut buffer = Vec::new();
        
        // ❌ Blocking operation
        std::thread::sleep(Duration::from_millis(1));
        
        // ❌ Mutex (can block)
        let _guard = self.mutex.lock().unwrap();
        
        // ❌ System call
        println!("Processing audio");
        
        AudioCallbackResult::Continue
    }
}
```

## Testing Guidelines

### Test Categories

1. **Unit Tests**: Fast, isolated component tests
2. **Integration Tests**: Multi-component interaction tests
3. **Performance Tests**: Latency and throughput validation
4. **Platform Tests**: Cross-platform compatibility
5. **Real-Time Tests**: Audio callback timing validation

### Writing Tests

#### Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use assert_approx_eq::assert_approx_eq;
    
    #[test]
    fn test_fft_forward_inverse() {
        let mut fft = Fft::new(1024).unwrap();
        let input: Vec<f32> = (0..1024).map(|i| (i as f32 * 0.1).sin()).collect();
        
        // Forward FFT
        let spectrum = fft.forward(&input).unwrap();
        
        // Inverse FFT
        let reconstructed = fft.inverse(&spectrum).unwrap();
        
        // Verify round-trip accuracy
        for (orig, recon) in input.iter().zip(reconstructed.iter()) {
            assert_approx_eq!(orig, recon, 1e-6);
        }
    }
    
    #[test]
    fn test_audio_latency_measurement() {
        let mut monitor = LatencyMonitor::new();
        
        // Simulate audio callback timing
        for _ in 0..1000 {
            let start = Instant::now();
            std::thread::sleep(Duration::from_micros(500)); // Simulate processing
            monitor.record_latency(start.elapsed());
        }
        
        let stats = monitor.get_statistics();
        assert!(stats.average_ms() < 1.0);
        assert!(stats.max_ms() < 2.0);
        assert_eq!(stats.sample_count(), 1000);
    }
}
```

#### Integration Tests

```rust
// tests/integration/audio_engine.rs
use pancetta_audio::{AudioEngine, AudioConfig, TestCallback};
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn test_audio_engine_startup_shutdown() {
    let config = AudioConfig {
        sample_rate: 48000,
        buffer_size: 64,
        ..Default::default()
    };
    
    let engine = AudioEngine::new(config).unwrap();
    let callback = TestCallback::new();
    
    // Start audio processing
    engine.start(Box::new(callback.clone())).unwrap();
    
    // Wait for audio callbacks to begin
    timeout(Duration::from_secs(1), async {
        while callback.process_count() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.unwrap();
    
    // Verify latency requirements
    let stats = callback.get_latency_stats();
    assert!(stats.average_ms() < 1.0, "Average latency too high: {:.2}ms", stats.average_ms());
    
    // Clean shutdown
    engine.stop().unwrap();
}
```

#### Performance Tests

```rust
// benches/audio_performance.rs
use criterion::{black_box, criterion_group, criterion_main, Criterion};
use pancetta_dsp::Fft;

fn benchmark_fft_performance(c: &mut Criterion) {
    let mut group = c.benchmark_group("fft");
    
    for size in [512, 1024, 2048, 4096].iter() {
        let mut fft = Fft::new(*size).unwrap();
        let input: Vec<f32> = (0..*size).map(|i| (i as f32 * 0.1).sin()).collect();
        
        group.bench_with_input(format!("fft_{}", size), size, |b, &size| {
            b.iter(|| {
                let spectrum = fft.forward(black_box(&input)).unwrap();
                black_box(spectrum);
            });
        });
    }
    
    group.finish();
}

criterion_group!(benches, benchmark_fft_performance);
criterion_main!(benches);
```

### Test Coverage

```bash
# Install cargo-tarpaulin for coverage
cargo install cargo-tarpaulin

# Generate coverage report
cargo tarpaulin --out html

# View coverage report
open tarpaulin-report.html
```

### Continuous Integration

Our CI pipeline runs:

1. **Code formatting** check (`cargo fmt`)
2. **Linting** with Clippy (`cargo clippy`)
3. **Unit tests** on all platforms (`cargo test`)
4. **Integration tests** with real audio hardware
5. **Performance benchmarks** with regression detection
6. **Documentation** build (`cargo doc`)
7. **Security audit** (`cargo audit`)

## Performance Requirements

### Real-Time Audio Constraints

**Audio Callback Thread:**
- **Maximum latency**: <1ms (target: 0.5ms)
- **No heap allocations** in audio thread
- **No blocking operations** (mutex, I/O, sleep)
- **No panics** - use `Result` types for error handling
- **Bounded processing time** - no unbounded loops

**Memory Management:**
- Pre-allocate all buffers during initialization
- Use lock-free data structures for thread communication
- Avoid `Vec::push` or any dynamic allocation in hot paths
- Pool objects that need temporary allocation

### Performance Testing

```rust
// Add performance tests for critical paths
#[test]
fn test_audio_callback_performance() {
    let mut processor = AudioProcessor::new();
    let input = vec![0.0f32; 1024];
    let mut output = vec![0.0f32; 1024];
    
    // Measure processing time
    let start = Instant::now();
    
    for _ in 0..1000 {
        processor.process(&input, &mut output);
    }
    
    let elapsed = start.elapsed();
    let avg_per_call = elapsed / 1000;
    
    // Assert performance requirement
    assert!(avg_per_call < Duration::from_micros(500), 
            "Processing too slow: {:.2}μs", avg_per_call.as_micros());
}
```

### Platform-Specific Optimizations

```rust
// Use target-specific optimizations where beneficial
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::*;

// SIMD-optimized processing
fn process_samples_optimized(input: &[f32], output: &mut [f32], gain: f32) {
    #[cfg(target_feature = "avx2")]
    {
        process_with_avx2(input, output, gain);
    }
    #[cfg(not(target_feature = "avx2"))]
    {
        process_scalar(input, output, gain);
    }
}
```

## Documentation

### Documentation Requirements

All public APIs must have:
- **Summary**: Brief description of purpose
- **Arguments**: Description of all parameters
- **Returns**: Description of return value and error conditions
- **Examples**: Working code examples
- **Performance Notes**: Any performance implications
- **Safety Notes**: Thread safety and real-time constraints

### Documentation Commands

```bash
# Build documentation
cargo doc --open

# Build with private items
cargo doc --document-private-items --open

# Test documentation examples
cargo test --doc

# Check for broken links
cargo doc --no-deps && cargo deadlinks
```

### README Guidelines

Each crate should have a README.md with:
- Purpose and scope
- Quick start example
- Performance characteristics
- Platform-specific notes
- Link to full documentation

## Issue Guidelines

### Bug Reports

When reporting bugs, include:

1. **Environment**:
   - OS and version
   - Rust version (`rustc --version`)
   - Pancetta version
   - Audio hardware details

2. **Reproduction Steps**:
   - Minimal example to reproduce
   - Expected vs actual behavior
   - Error messages or logs

3. **Audio-Specific Issues**:
   - Sample rate and buffer size
   - Latency measurements
   - Audio device information

### Feature Requests

For new features, provide:

1. **Use Case**: Why is this feature needed?
2. **Proposed API**: How should it work?
3. **Performance Impact**: Any real-time constraints?
4. **Alternatives**: Other approaches considered?

### Issue Template

```markdown
**Bug Report / Feature Request**

**Environment:**
- OS: [e.g., Ubuntu 22.04, macOS 13.0, Windows 11]
- Rust version: [output of `rustc --version`]
- Pancetta version: [e.g., 0.1.0]
- Audio hardware: [e.g., Focusrite Scarlett 2i2, Built-in audio]

**Description:**
[Clear description of the issue or feature request]

**Reproduction Steps:**
1. [First step]
2. [Second step]
3. [See error]

**Expected Behavior:**
[What you expected to happen]

**Actual Behavior:**
[What actually happened]

**Additional Context:**
[Any other context, logs, or screenshots]
```

## Pull Request Process

### PR Guidelines

1. **One Feature Per PR**: Keep changes focused and atomic
2. **Descriptive Title**: Use conventional commit format
3. **Complete Description**: Explain what, why, and how
4. **Tests Included**: All new code must have tests
5. **Documentation Updated**: Update docs for API changes
6. **Performance Impact**: Note any performance implications

### PR Template

```markdown
**Summary**
Brief description of changes

**Changes Made**
- [ ] Feature implementation
- [ ] Tests added/updated
- [ ] Documentation updated
- [ ] Performance validated

**Testing**
- [ ] Unit tests pass
- [ ] Integration tests pass
- [ ] Performance benchmarks run
- [ ] Manual testing completed

**Performance Impact**
[Describe any performance implications]

**Breaking Changes**
[List any breaking changes]

**Related Issues**
Closes #123
```

### PR Review Process

1. **Automated Checks**: CI must pass
2. **Code Review**: At least one maintainer approval
3. **Performance Review**: For audio/DSP changes
4. **Documentation Review**: For API changes
5. **Final Testing**: Manual verification if needed

### Conventional Commits

We use conventional commit format:

```
type(scope): description

[optional body]

[optional footer]
```

**Types:**
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation changes
- `style`: Code formatting
- `refactor`: Code restructuring
- `perf`: Performance improvements
- `test`: Test additions/changes
- `chore`: Maintenance tasks

**Examples:**
```
feat(audio): add JACK backend support

Implement JACK audio backend for Linux systems requiring
professional audio routing capabilities.

- Add JackEngine implementation
- Update AudioBackend enum
- Add JACK-specific configuration options
- Include integration tests

Closes #45

fix(ft8): correct time synchronization drift

Address timing drift in FT8 decoder that caused decode
failures after extended operation.

perf(dsp): optimize FFT with SIMD instructions

Replace scalar FFT implementation with AVX2 SIMD version,
improving performance by 40% on x86_64 systems.

Benchmarks:
- 1024-point FFT: 45μs → 27μs
- 2048-point FFT: 92μs → 55μs
```

## Release Process

### Version Numbering

We follow [Semantic Versioning](https://semver.org/):

- **MAJOR**: Breaking API changes
- **MINOR**: New features, backward compatible
- **PATCH**: Bug fixes, backward compatible

### Release Steps

1. **Update Version Numbers**:
   ```bash
   # Update Cargo.toml files
   scripts/update-version.sh 0.2.0
   ```

2. **Update CHANGELOG.md**:
   - Add new version section
   - List all changes since last release
   - Credit contributors

3. **Create Release PR**:
   ```bash
   git checkout -b release/v0.2.0
   git commit -m "chore: prepare v0.2.0 release"
   git push origin release/v0.2.0
   ```

4. **Tag Release**:
   ```bash
   git tag -a v0.2.0 -m "Release v0.2.0"
   git push upstream v0.2.0
   ```

5. **Publish Crates**:
   ```bash
   scripts/publish-crates.sh
   ```

6. **Create GitHub Release**:
   - Upload binary artifacts
   - Include changelog excerpt
   - Mark as latest release

### Release Checklist

- [ ] All tests pass on all platforms
- [ ] Performance benchmarks meet requirements
- [ ] Documentation is up to date
- [ ] CHANGELOG.md updated
- [ ] Version numbers bumped
- [ ] Release notes prepared
- [ ] Binary artifacts built
- [ ] Security audit passed

## Community and Communication

### Getting Help

- **GitHub Discussions**: General questions and ideas
- **GitHub Issues**: Bug reports and feature requests  
- **Matrix Chat**: Real-time discussions (#pancetta:matrix.org)
- **Email**: Direct contact (team@pancetta.dev)

### Contributing Areas

We welcome contributions in many areas:

**Core Development:**
- Real-time audio processing improvements
- FT8 protocol enhancements
- Digital signal processing optimizations
- Cross-platform compatibility

**User Experience:**
- Terminal UI improvements
- Configuration management
- Error handling and diagnostics
- Documentation and tutorials

**Platform Support:**
- Audio driver integration
- Package management
- Performance optimization
- Testing infrastructure

**Community:**
- Documentation improvements
- Tutorial creation
- Bug reports and testing
- Feature suggestions

### Recognition

Contributors are recognized in:
- CHANGELOG.md for each release
- GitHub repository contributors list
- Project website (when available)
- Conference presentations and talks

We appreciate all forms of contribution, from code to documentation to testing and feedback!

---

**Thank you for contributing to Pancetta!** 

Your contributions help make amateur radio digital communications more accessible and performant for operators worldwide. If you have questions about contributing, please don't hesitate to reach out through any of our communication channels.

*Happy coding and 73!* 📻