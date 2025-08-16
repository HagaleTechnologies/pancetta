# ADR-001: Programming Language Selection

## Status
Accepted

## Context
Pancetta requires a programming language that can handle real-time audio processing, integrate with C libraries (hamlib), support cross-platform development, and attract open-source contributors.

## Decision
We will use **Rust** as the primary programming language for Pancetta.

## Consequences

### Positive
- Memory safety without garbage collection ensures predictable latency
- Excellent FFI support for hamlib integration
- Strong type system catches errors at compile time
- Growing ecosystem with good audio libraries (cpal)
- Single binary deployment simplifies distribution
- Active ham radio community (rustradio projects)

### Negative
- Steeper learning curve than Go or Python
- Longer compile times during development
- Smaller developer pool than mainstream languages
- Some libraries may be less mature

## Alternatives Considered

### Go
- ✅ Simple language, easy to learn
- ✅ Good cross-platform support
- ❌ Garbage collector causes latency spikes
- ❌ Less mature audio ecosystem

### Python
- ✅ Large ecosystem and community
- ✅ Rapid development
- ❌ Too slow for real-time audio processing
- ❌ Complex distribution (dependencies)

### C++20
- ✅ Maximum performance
- ✅ Mature ecosystem
- ❌ Complex memory management
- ❌ Harder to attract contributors

## References
- [Rust Audio Working Group](https://github.com/RustAudio)
- [cpal - Cross-platform audio](https://github.com/RustAudio/cpal)
- [hamlib-rs - Rust bindings](https://github.com/N5FPP/hamlib-rs)