//! Core type definitions shared across all Pancetta modules

pub mod mode;
pub mod mode_v2;
pub mod band;
pub mod error;

pub use mode::Mode;
pub use mode_v2::{ModeValue, StandardMode};
pub use band::Band;
pub use error::{PancettaError, PancettaResult};