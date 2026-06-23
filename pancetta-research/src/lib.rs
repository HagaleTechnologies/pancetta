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

pub mod metrics;

pub mod bootstrap_ci;
pub use bootstrap_ci::{bootstrap_novel_delta, bootstrap_recall_delta, DeltaCi};

pub mod decoder;
pub use decoder::{Decode, DecoderUnderTest, Ft8Decoder, Jt9Decoder};

pub mod corpus;

pub mod truth;

pub mod synth;

pub mod noise;

pub mod curated;

pub mod chrono_replay;

pub mod fp_filter;
pub use fp_filter::FpFilter;

pub mod callsign_priors;
pub use callsign_priors::{CallsignPriorSet, PriorSourceMask, BUNDLED_COMMON_ACTIVE};

pub mod tier_slots;
pub use tier_slots::{is_heavy_tier, SlotGuard, TierSlotPool, DEFAULT_POOL_DIR};
