//! # pancetta-cqdx
//!
//! cqdx.io HTTP client and in-memory cache — entities, spots, and rarity scores.
//!
//! Provides [`CqdxClient`] for HTTP communication with cqdx.io
//! and [`CqdxCache`] for in-memory session caching of entities,
//! needed status, and rarity scores.
//!
//! ## Data Flow
//! cqdx.io REST API -> **pancetta-cqdx** -> `pancetta` coordinator (entities, spots, rarity data)
//!
//! ## Key Types
//! - [`CqdxClient`] -- async HTTP client for the cqdx.io API
//! - [`CqdxCache`] -- in-memory cache of entities, needed status, and rarity scores keyed by band
//! - [`CqdxError`] -- HTTP, parse, and cache errors
//! - `frequency_to_band` -- maps an audio frequency (Hz) to an amateur band string
//!
//! ## Crate Relationships
//! - Receives from: cqdx.io REST API (HTTP)
//! - Sends to: `pancetta` coordinator (entities, live spots, rarity scores)

#![warn(missing_docs)]

pub mod cache;
pub mod client;
pub mod error;
pub mod types;

pub use cache::{frequency_to_band, CqdxCache};
pub use client::CqdxClient;
pub use error::{CqdxError, Result};
pub use types::*;
