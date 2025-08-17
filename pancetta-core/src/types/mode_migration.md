# Mode Type Migration Guide

## Overview

This document describes the migration from the original `Mode` enum to the new thread-safe `ModeValue` type that supports custom modes.

## Problem Statement

The original `Mode` enum had these limitations:
- Could not support custom/unknown modes (fixed set of variants)
- Required manual updates to add new digital modes
- No extensibility for experimental or proprietary modes

## Solution Architecture

### Design Principles

1. **Thread Safety**: Maintain `Send + Sync` traits for async operations
2. **Efficiency**: Minimize memory overhead and cloning costs
3. **Compatibility**: Easy migration path from existing code
4. **Extensibility**: Support arbitrary custom modes
5. **Performance**: Efficient equality comparisons and hashing

### Implementation Approach

The new `ModeValue` type uses:
- **Arc-wrapped internal representation** for cheap cloning
- **Enum-based discrimination** between standard and custom modes
- **String storage** for custom mode names
- **Automatic Send+Sync** via Arc's thread-safe reference counting

### Memory Characteristics

| Aspect | Original Mode | New ModeValue |
|--------|--------------|---------------|
| Stack Size | 1 byte | 8 bytes (Arc pointer) |
| Clone Cost | O(1) copy | O(1) Arc increment |
| Thread Transfer | Copy | Arc clone |
| Custom Support | No | Yes |
| Memory Allocation | Stack only | Stack + optional heap for custom |

## Migration Steps

### 1. Update Imports

```rust
// Old
use pancetta_core::Mode;

// New
use pancetta_core::ModeValue;
// Or if using both during migration:
use pancetta_core::{Mode, ModeValue};
```

### 2. Update Type Declarations

```rust
// Old
struct RigState {
    mode: Mode,
}

// New
struct RigState {
    mode: ModeValue,
}
```

### 3. Update Pattern Matching

```rust
// Old
match mode {
    Mode::FT8 => process_ft8(),
    Mode::USB => process_usb(),
    _ => process_other(),
}

// New
match mode.as_standard() {
    Some(StandardMode::FT8) => process_ft8(),
    Some(StandardMode::USB) => process_usb(),
    _ => {
        if let Some(custom) = mode.as_custom() {
            process_custom(custom);
        } else {
            process_other();
        }
    }
}
```

### 4. Update Comparisons

```rust
// Old
if mode == Mode::FT8 {
    // ...
}

// New (Option 1: Direct comparison)
if mode == ModeValue::standard(StandardMode::FT8) {
    // ...
}

// New (Option 2: Check standard mode)
if mode.as_standard() == Some(StandardMode::FT8) {
    // ...
}
```

### 5. Handle Custom Modes

```rust
// Parse potentially custom mode from string
let mode = "OLIVIA-16/500".parse::<ModeValue>().unwrap();
if mode.is_custom() {
    println!("Custom mode: {}", mode.as_custom().unwrap());
}

// Create custom mode explicitly
let custom_mode = ModeValue::custom("VARA-HF");
```

## Performance Considerations

### Threading

The new `ModeValue` is optimized for multi-threaded use:
- **Arc reference counting** is atomic and thread-safe
- **Cloning is cheap** (just increments reference count)
- **No locks required** for read access
- **Immutable after creation** prevents data races

### Memory Usage

- Standard modes: ~24 bytes heap allocation (Arc control block + enum)
- Custom modes: ~24 bytes + string length
- Clone operation: No heap allocation, just Arc increment

### Comparison Performance

- **Equality check**: First tries pointer equality (very fast), then value equality
- **Hashing**: Consistent and suitable for HashMap/HashSet
- **Pattern matching**: Direct for standard modes, string comparison for custom

## Example Usage

### Basic Usage

```rust
use pancetta_core::{ModeValue, StandardMode};

// Create standard modes
let ft8 = ModeValue::standard(StandardMode::FT8);
let usb = ModeValue::standard(StandardMode::USB);

// Create custom mode
let olivia = ModeValue::custom("OLIVIA-16/500");

// Check mode type
assert!(ft8.is_standard());
assert!(olivia.is_custom());

// Get properties
assert_eq!(ft8.default_bandwidth(), Some(50));
assert!(ft8.is_digital());
```

### Thread-Safe Sharing

```rust
use std::sync::Arc;
use std::thread;
use pancetta_core::ModeValue;

let mode = ModeValue::custom("VARA-HF");

// Share across threads
let mode1 = mode.clone();
let handle1 = thread::spawn(move || {
    println!("Thread 1: {}", mode1);
});

let mode2 = mode.clone();
let handle2 = thread::spawn(move || {
    println!("Thread 2: {}", mode2);
});

handle1.join().unwrap();
handle2.join().unwrap();
```

### Async Operations

```rust
use pancetta_core::ModeValue;
use tokio::sync::mpsc;

#[tokio::main]
async fn main() {
    let (tx, mut rx) = mpsc::channel(100);
    
    // Send mode across async channel
    let mode = ModeValue::custom("JS8SLOW");
    tx.send(mode).await.unwrap();
    
    // Receive in another task
    tokio::spawn(async move {
        while let Some(mode) = rx.recv().await {
            println!("Received mode: {}", mode);
        }
    });
}
```

## Backward Compatibility

A conversion is provided from the old `Mode` to new `ModeValue`:

```rust
use pancetta_core::{Mode, ModeValue};

let old_mode = Mode::FT8;
let new_mode: ModeValue = old_mode.into();
```

This allows gradual migration of codebases.

## Testing Strategy

1. **Unit Tests**: Test all mode types and conversions
2. **Thread Safety Tests**: Verify Send+Sync behavior
3. **Performance Tests**: Benchmark cloning and comparisons
4. **Integration Tests**: Test with real async/multi-threaded code
5. **Backward Compatibility Tests**: Ensure old Mode converts correctly

## Rollout Plan

### Phase 1: Parallel Implementation
- Add new `ModeValue` alongside existing `Mode`
- Provide conversion methods between types
- No breaking changes

### Phase 2: Gradual Migration
- Update internal modules to use `ModeValue`
- Maintain compatibility layer for external API
- Deprecate old `Mode` methods

### Phase 3: Complete Migration
- Remove deprecated `Mode` enum
- Update all documentation
- Release as major version bump

## Alternative Approaches Considered

### 1. String Interning (Not Chosen)
- **Pros**: Maintains Copy trait, memory efficient for repeated custom modes
- **Cons**: Complex global state, harder to implement correctly

### 2. Cow<'static, str> (Not Chosen)
- **Pros**: Efficient for static strings
- **Cons**: Lifetime complications, not truly owned

### 3. SmallString Optimization (Not Chosen)
- **Pros**: Avoids heap allocation for short names
- **Cons**: Still can't be Copy, more complex implementation

## Conclusion

The Arc-based `ModeValue` provides the best balance of:
- Thread safety without locks
- Efficient cloning and sharing
- Support for arbitrary custom modes
- Clean API and easy migration
- Minimal performance impact

This design enables Pancetta to handle both standard amateur radio modes and experimental/proprietary modes while maintaining thread safety for real-time audio processing.