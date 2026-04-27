//! # pancetta-hamlib
//!
//! Hamlib CAT control over `rigctld` — frequency, mode, and PTT for the
//! Yaesu FTdx10 and any other rig hamlib supports.
//!
//! Two paths used to live in this crate: a native libhamlib FFI binding
//! and a `rigctld` TCP client. The FFI path was never validated on real
//! hardware and has been removed; what's left is the active rigctld path
//! plus a pure-Rust mock for unit tests.
//!
//! ## Quick start
//!
//! ```no_run
//! use pancetta_hamlib::{RigctldClient, RigctldConfig, RigControl, Vfo};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let rig = RigctldClient::new(RigctldConfig {
//!         host: "127.0.0.1".to_string(),
//!         port: 4532,
//!         ..Default::default()
//!     });
//!     rig.connect().await?;
//!     let freq = rig.get_frequency(Vfo::Current).await?;
//!     println!("On {:.3} MHz", freq as f64 / 1_000_000.0);
//!     Ok(())
//! }
//! ```
//!
//! ## Mock rig for testing
//!
//! ```
//! use pancetta_hamlib::{MockRig, RigControl, Vfo};
//!
//! #[tokio::main]
//! async fn main() -> Result<(), Box<dyn std::error::Error>> {
//!     let rig = MockRig::default();
//!     rig.connect().await?;
//!     rig.set_frequency(Vfo::A, 14_200_000).await?;
//!     assert_eq!(rig.get_frequency(Vfo::A).await?, 14_200_000);
//!     Ok(())
//! }
//! ```

#![deny(missing_docs)]
#![warn(clippy::all)]

pub mod error;
pub mod models;
pub mod rig;
pub mod rigctld;

#[cfg(feature = "mock-rig")]
pub mod mock;

// Public surface — what the rest of the workspace depends on.
pub use error::{ContextualError, ContextualResult, ErrorContext, ErrorSeverity, HamlibError};
pub use models::{Band, Mode, RigCapabilities, RigModelType, Vfo};
pub use rig::{ConnectionState, PttState, RigConfig, RigControl, RigStatus};
pub use rigctld::{RigctldClient, RigctldConfig};

#[cfg(feature = "mock-rig")]
pub use mock::{MockRig, MockRigConfig};

/// Convenience `Result` alias bound to [`HamlibError`].
pub type HamlibResult<T> = std::result::Result<T, HamlibError>;

/// Crate version, taken from `Cargo.toml` at compile time.
pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_version() {
        let v = version();
        assert!(!v.is_empty());
        assert!(v.chars().any(|c| c.is_ascii_digit()));
    }
}
