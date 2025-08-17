# Contributing to Pancetta

Thank you for your interest in contributing to Pancetta! We welcome contributions from the amateur radio and Rust communities.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Getting Started](#getting-started)
- [Development Setup](#development-setup)
- [Contribution Process](#contribution-process)
- [Coding Standards](#coding-standards)
- [Testing Requirements](#testing-requirements)
- [Documentation](#documentation)
- [Submitting Changes](#submitting-changes)
- [Review Process](#review-process)

## Code of Conduct

### Our Pledge

We pledge to make participation in our project a harassment-free experience for everyone, regardless of:
- Age, body size, disability, ethnicity, gender identity
- Level of experience, nationality, personal appearance
- Race, religion, or sexual identity and orientation

### Expected Behavior

- Use welcoming and inclusive language
- Be respectful of differing viewpoints
- Accept constructive criticism gracefully
- Focus on what's best for the community
- Show empathy towards other community members

### Unacceptable Behavior

- Harassment, trolling, or derogatory comments
- Personal or political attacks
- Public or private harassment
- Publishing others' private information
- Other conduct which could reasonably be considered inappropriate

## Getting Started

### Prerequisites

Before contributing, ensure you have:

1. **Rust 1.70+**: Install from [rustup.rs](https://rustup.rs/)
2. **Git**: For version control
3. **GitHub Account**: For submitting PRs
4. **Discord** (optional): Join our community for discussions

### Fork and Clone

```bash
# Fork the repository on GitHub
# Then clone your fork
git clone https://github.com/YOUR_USERNAME/pancetta.git
cd pancetta

# Add upstream remote
git remote add upstream https://github.com/pancetta-project/pancetta.git

# Verify remotes
git remote -v
```

## Development Setup

### Building the Project

```bash
# Build all packages
cargo build --all

# Build in release mode
cargo build --release

# Run tests
cargo test --all

# Run with debug logging
RUST_LOG=debug cargo run --bin pancetta
```

### Development Environment

#### VS Code

```json
// .vscode/settings.json
{
  "rust-analyzer.cargo.features": "all",
  "rust-analyzer.checkOnSave.command": "clippy",
  "editor.formatOnSave": true
}
```

#### Vim/Neovim

```vim
" Install rust.vim
Plug 'rust-lang/rust.vim'

" Format on save
let g:rustfmt_autosave = 1
```

## Contribution Process

### 1. Find or Create an Issue

Before starting work:

- Check [existing issues](https://github.com/pancetta-project/pancetta/issues)
- If none exist, create a new issue describing:
  - The problem or feature
  - Your proposed solution
  - Any design considerations

### 2. Create a Feature Branch

```bash
# Update your fork
git checkout main
git pull upstream main
git push origin main

# Create feature branch
git checkout -b feature/your-feature-name
# or
git checkout -b fix/issue-description
```

### 3. Make Your Changes

Follow these guidelines:

- Write clean, idiomatic Rust code
- Add tests for new functionality
- Update documentation as needed
- Keep commits atomic and well-described

### 4. Test Your Changes

```bash
# Run all tests
cargo test --all

# Run specific test
cargo test test_name

# Run benchmarks
cargo bench

# Check formatting
cargo fmt --all -- --check

# Run clippy
cargo clippy --all -- -D warnings

# Test with different features
cargo test --no-default-features
cargo test --all-features
```

### 5. Commit Your Changes

Follow conventional commit format:

```bash
# Format: <type>(<scope>): <subject>

# Examples:
git commit -m "feat(ft8): add deep search algorithm"
git commit -m "fix(audio): resolve buffer overflow"
git commit -m "docs(readme): update installation instructions"
git commit -m "test(dsp): add AGC unit tests"
git commit -m "perf(decoder): optimize LDPC decoding"
```

Types:
- `feat`: New feature
- `fix`: Bug fix
- `docs`: Documentation only
- `style`: Formatting, missing semicolons, etc.
- `refactor`: Code change that neither fixes a bug nor adds a feature
- `perf`: Performance improvement
- `test`: Adding missing tests
- `chore`: Maintenance tasks

## Coding Standards

### Rust Style Guide

Follow the official [Rust Style Guide](https://doc.rust-lang.org/style-guide/) and these additional conventions:

```rust
// Good: Descriptive names
pub struct AudioProcessor {
    sample_rate: u32,
    buffer_size: usize,
}

// Good: Error handling
fn process_audio(data: &[f32]) -> Result<Vec<f32>, AudioError> {
    // Implementation
}

// Good: Documentation
/// Processes audio samples through the DSP pipeline.
///
/// # Arguments
///
/// * `samples` - Input audio samples
/// * `config` - DSP configuration
///
/// # Returns
///
/// Processed audio samples or error
pub fn process_samples(
    samples: &[f32],
    config: &DspConfig,
) -> Result<Vec<f32>, DspError> {
    // Implementation
}
```

### Performance Considerations

For real-time audio code:

```rust
// GOOD: Pre-allocate buffers
let mut buffer = Vec::with_capacity(BUFFER_SIZE);

// BAD: Dynamic allocation in hot path
let buffer = vec![0.0; size]; // Avoid in audio callback

// GOOD: Use iterators
samples.iter()
    .map(|s| s * gain)
    .collect()

// GOOD: Avoid panics in real-time code
if let Some(value) = optional_value {
    // Process
} else {
    // Handle gracefully
}
```

### Documentation Standards

All public APIs must be documented:

```rust
/// FT8 decoder with configurable parameters.
///
/// This decoder implements the WSJT-X FT8 protocol with
/// optimizations for weak signal decoding.
///
/// # Example
///
/// ```
/// use pancetta_ft8::Decoder;
///
/// let decoder = Decoder::new(DecoderConfig::default());
/// let messages = decoder.decode(&samples)?;
/// ```
pub struct Decoder {
    // ...
}
```

## Testing Requirements

### Unit Tests

Place unit tests in the same file as the code:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_buffer_creation() {
        let buffer = AudioBuffer::new(1024);
        assert_eq!(buffer.capacity(), 1024);
    }
}
```

### Integration Tests

Place integration tests in `tests/` directory:

```rust
// tests/integration_test.rs
use pancetta::*;

#[test]
fn test_full_pipeline() {
    // Test complete audio -> DSP -> FT8 pipeline
}
```

### Performance Tests

```rust
// benches/performance.rs
use criterion::{black_box, criterion_group, criterion_main, Criterion};

fn bench_ft8_decode(c: &mut Criterion) {
    c.bench_function("ft8_decode", |b| {
        b.iter(|| decode_ft8(black_box(&samples)))
    });
}
```

### Test Coverage

Aim for >80% test coverage:

```bash
# Install cargo-tarpaulin
cargo install cargo-tarpaulin

# Generate coverage report
cargo tarpaulin --out Html
```

## Documentation

### Code Comments

```rust
// Good: Explain WHY, not WHAT
// We use a ring buffer here to avoid allocations
// in the real-time audio callback
let buffer = RingBuffer::new(1024);

// Bad: Redundant comment
// Create a new buffer with size 1024
let buffer = Buffer::new(1024);
```

### README Updates

Update README.md when:
- Adding new features
- Changing installation steps
- Modifying configuration options
- Updating performance metrics

### API Documentation

```bash
# Generate and view docs
cargo doc --open

# Generate docs with private items
cargo doc --document-private-items
```

## Submitting Changes

### Pull Request Template

```markdown
## Description
Brief description of changes

## Type of Change
- [ ] Bug fix
- [ ] New feature
- [ ] Breaking change
- [ ] Documentation update

## Testing
- [ ] Unit tests pass
- [ ] Integration tests pass
- [ ] Manual testing completed

## Checklist
- [ ] Code follows style guidelines
- [ ] Self-review completed
- [ ] Documentation updated
- [ ] No new warnings
```

### Before Submitting

1. **Update from upstream**:
   ```bash
   git fetch upstream
   git rebase upstream/main
   ```

2. **Run full test suite**:
   ```bash
   ./scripts/pre-submit.sh
   ```

3. **Squash commits if needed**:
   ```bash
   git rebase -i HEAD~n
   ```

4. **Push to your fork**:
   ```bash
   git push origin feature/your-feature
   ```

5. **Create Pull Request** on GitHub

## Review Process

### What to Expect

1. **Automated Checks**: CI will run tests, formatting, and linting
2. **Code Review**: Maintainers will review within 48 hours
3. **Feedback**: Address any requested changes
4. **Approval**: Two approvals required for merge
5. **Merge**: Maintainer will merge when ready

### Review Criteria

- **Correctness**: Does it work as intended?
- **Performance**: No regressions in critical paths
- **Style**: Follows project conventions
- **Testing**: Adequate test coverage
- **Documentation**: Clear and complete

### After Merge

```bash
# Update your local main
git checkout main
git pull upstream main

# Delete feature branch
git branch -d feature/your-feature
git push origin --delete feature/your-feature
```

## Getting Help

### Resources

- **Discord**: [Join our server](https://discord.gg/pancetta)
- **Documentation**: [docs/](./docs/)
- **Issues**: [GitHub Issues](https://github.com/pancetta-project/pancetta/issues)
- **Discussions**: [GitHub Discussions](https://github.com/pancetta-project/pancetta/discussions)

### Maintainers

- Project Lead: [@username](https://github.com/username)
- DSP Expert: [@username](https://github.com/username)
- FT8 Specialist: [@username](https://github.com/username)

## Recognition

Contributors are recognized in:
- [CONTRIBUTORS.md](../CONTRIBUTORS.md)
- Release notes
- Project documentation

## License

By contributing, you agree that your contributions will be licensed under the same license as the project (MIT).

---

**Thank you for contributing to Pancetta! Your efforts help advance amateur radio technology.**

73 de Pancetta Team 📻