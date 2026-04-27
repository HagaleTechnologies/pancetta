//! # pancetta-core
//!
//! Shared types, errors, and utilities used by every other crate in the workspace.
//!
//! This crate provides the fundamental types and utilities that are shared
//! across all Pancetta modules, ensuring consistency and reducing duplication.
//!
//! ## Data Flow
//! (no upstream) -> **pancetta-core** -> every other crate
//!
//! ## Key Types
//! - [`Band`] -- amateur radio band (20m, 40m, etc.)
//! - [`Mode`] -- operating mode (FT8, USB, etc.)
//! - [`PancettaError`] -- unified error type
//! - [`ComponentError`] -- per-component error with severity and context
//!
//! ## Crate Relationships
//! - Receives from: nothing (foundational layer)
//! - Sends to: all crates (`pancetta-audio`, `pancetta-ft8`, `pancetta-dsp`,
//!   `pancetta-config`, `pancetta-qso`, `pancetta-hamlib`, `pancetta-dx`,
//!   `pancetta-cqdx`, `pancetta-tui`, `pancetta`)

#![allow(missing_docs)] // TODO: re-enable as docs are added

pub mod error;
pub mod slot;
pub mod types;

// Re-export core types at the crate root for convenience
pub use types::{Band, Mode, ModeValue, PancettaError, PancettaResult, StandardMode};

// Re-export error handling types
pub use error::{
    ComponentError, ErrorBuilder, ErrorContext as NewErrorContext, ErrorRecovery, ErrorSeverity,
    PancettaError as UnifiedError, PancettaResult as UnifiedResult, ResultExt, RetryDelay,
};

// Re-export error context trait
pub use types::error::ErrorContext;

// Version information
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const VERSION_MAJOR: u32 = 0;
pub const VERSION_MINOR: u32 = 1;
pub const VERSION_PATCH: u32 = 0;

/// Get full version string with build metadata
pub fn version_string() -> String {
    format!(
        "{} ({})",
        VERSION,
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        assert!(!VERSION.is_empty());
        assert!(version_string().contains(VERSION));
    }

    #[test]
    fn test_type_exports() {
        // Ensure types are accessible
        let _mode = Mode::FT8;
        let _band = Band::Band20m;
        let _err = PancettaError::Other("test".to_string());
    }
}
