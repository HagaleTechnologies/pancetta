//! Local-only research harness for the pancetta decoder.
//!
//! This crate is **excluded from the workspace `default-members`** and never
//! built in CI. See `pancetta-research/README.md` and
//! `docs/superpowers/specs/2026-05-18-decoder-research-harness-design.md`.

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md

pub mod mode;
pub use mode::Mode;

pub mod scorecard;
pub use scorecard::Scorecard;
