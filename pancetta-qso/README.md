# Pancetta QSO Management Library

A comprehensive QSO (contact) management and logging library for FT8 amateur radio communications. This library provides state machine-based QSO tracking, automatic sequencing, ADIF import/export, SQLite-based storage, and comprehensive statistics and analytics.

## Features

- **QSO State Machine**: Complete FT8 QSO flow management with automatic state transitions
- **Auto Sequencing**: Intelligent automatic QSO progression with configurable behavior
- **ADIF 3.0 Support**: Full ADIF import/export with validation and conversion
- **SQLite Database**: Efficient storage with advanced querying and indexing
- **Comprehensive Logging**: Automatic logging with duplicate detection and validation
- **Statistics & Analytics**: Detailed QSO statistics, trends, and achievement tracking
- **Contest Support**: Contest-specific QSO handling and tracking
- **Message Exchange**: FT8 message parsing and generation with validation

## Quick Start

Add this to your `Cargo.toml`:

```toml
[dependencies]
pancetta-qso = "0.1.0"
```

### Basic Usage

```rust
use pancetta_qso::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create QSO system
    let system = QsoSystem::new("W1ABC".to_string(), Some("FN42".to_string())).await?;
    
    // Start a CQ call
    let qso_id = system.start_cq(14074000.0).await?;
    println!("Started CQ: {}", qso_id);
    
    // Process incoming messages
    system.process_message(
        MessageType::CqResponse {
            calling_station: "W1ABC".to_string(),
            responding_station: "K1DEF".to_string(),
            grid: Some("FN31".to_string()),
        },
        "W1ABC K1DEF FN31".to_string(),
        14074000.0,
        Some(-12.0),
    ).await?;
    
    Ok(())
}
```

### With Logging

```rust
use pancetta_qso::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create system with logging
    let system = QsoSystem::with_logging(
        "W1ABC".to_string(),
        Some("FN42".to_string()),
        Some("qso.db".into()),
    ).await?;
    
    // QSOs are automatically logged when completed
    let qso_id = system.start_cq(14074000.0).await?;
    
    // Export to ADIF
    system.export_adif("my_qsos.adi", None).await?;
    
    Ok(())
}
```

### Full Featured System

```rust
use pancetta_qso::*;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Create full system with auto-sequencing and logging
    let system = QsoSystem::full_featured(
        "W1ABC".to_string(),
        Some("FN42".to_string()),
        Some("qso.db".into()),
    ).await?;
    
    // System will automatically respond to CQs and manage QSO flow
    println!("Full featured QSO system running!");
    
    Ok(())
}
```

## Architecture

The library is organized into several key modules:

- **`states`**: Core QSO state definitions and transitions
- **`qso_manager`**: QSO lifecycle management and state machine
- **`exchange`**: FT8 message parsing and generation
- **`auto_sequencer`**: Automatic QSO progression logic
- **`adif`**: ADIF 3.0 format support for import/export
- **`database`**: SQLite-based persistent storage
- **`logger`**: QSO logging with automatic features
- **`statistics`**: Comprehensive statistics and analytics

## QSO State Machine

The library implements a complete FT8 QSO state machine:

1. **Idle** - No active QSO
2. **Calling CQ** - Broadcasting CQ call
3. **Responding to CQ** - Answering someone's CQ
4. **Waiting for Report** - Expecting signal report
5. **Sending Report** - Transmitting our signal report
6. **Waiting for Confirmation** - Expecting RR73
7. **Sending Confirmation** - Transmitting final confirmation
8. **Completed** - QSO successfully finished
9. **Failed** - QSO failed or timed out

Each state transition is tracked with timestamps and reasons, providing complete QSO history.

## Message Types

The library supports all standard FT8 message types:

- **CQ calls**: `CQ W1ABC FN42`
- **CQ responses**: `W1ABC K1DEF FN31`
- **Signal reports**: `K1DEF W1ABC -15`
- **Report acknowledgments**: `W1ABC K1DEF R-12`
- **Final confirmations**: `K1DEF W1ABC RR73`
- **73 messages**: `W1ABC K1DEF 73`
- **Contest exchanges**: `W1ABC K1DEF 599 001`

## Automatic Sequencing

The auto sequencer can:

- Automatically respond to CQ calls based on configurable criteria
- Send signal reports and confirmations automatically
- Handle multiple concurrent QSOs
- Implement contest-specific logic
- Filter stations based on signal strength, location, etc.

## ADIF Support

Full ADIF 3.0 support includes:

- Import/export ADIF files
- Validate ADIF records
- Convert between internal format and ADIF
- Support for contest fields
- Custom field handling

## Database Features

SQLite database provides:

- Efficient QSO storage and retrieval
- Advanced filtering and searching
- Duplicate detection
- Statistics calculation
- Backup and restore
- Schema versioning

## Statistics and Analytics

Comprehensive statistics include:

- Basic counts (QSOs, countries, grids, etc.)
- Temporal analysis (activity patterns, trends)
- Geographic statistics (DXCC, WAS, WAZ progress)
- Technical metrics (signal reports, completion rates)
- Contest performance
- Achievement tracking
- Trend analysis and predictions

## Examples

See the `examples/` directory for detailed usage examples:

- `basic_qso.rs` - Basic QSO management
- `contest_logging.rs` - Contest-specific logging

Run examples with:

```bash
cargo run --example basic_qso
cargo run --example contest_logging
```

## Development

### Building

```bash
cargo build
```

### Testing

```bash
cargo test
```

### Documentation

```bash
cargo doc --open
```

## License

This project is licensed under either of

- Apache License, Version 2.0
- MIT License

at your option.

## Contributing

Contributions are welcome! Please feel free to submit a Pull Request.