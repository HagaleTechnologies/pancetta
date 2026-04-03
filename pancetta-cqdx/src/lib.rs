//! cqdx.io API client and cache for Pancetta.
//!
//! Provides `CqdxClient` for HTTP communication with cqdx.io
//! and `CqdxCache` for in-memory session caching of entities,
//! needed status, and rarity scores.

pub mod cache;
pub mod client;
pub mod error;
pub mod types;

pub use cache::CqdxCache;
pub use client::CqdxClient;
pub use error::{CqdxError, Result};
pub use types::*;
