//! # pancetta
//!
//! Main binary crate: coordinator, message bus, pipeline, and priority evaluator.
//! Orchestrates all other crates into a running FT8 station.
//!
//! This is the integration point for the entire workspace. The coordinator drives
//! the decode → decide → transmit loop, the message bus wires crates together,
//! and the priority evaluator selects which stations to work each 15-second slot.
//!
//! ## Data Flow
//! `pancetta-audio` -> `pancetta-dsp` -> `pancetta-ft8` -> **pancetta** coordinator
//! -> `pancetta-qso` (decisions) + `pancetta-hamlib` (PTT/freq) + `pancetta-tui` (display)
//!
//! ## Key Types
//! - [`coordinator::ApplicationCoordinator`] -- central orchestrator (~2 700 lines); owns the full pipeline
//! - [`message_bus::MessageBus`] -- async broadcast channel connecting all subsystems
//! - [`priority_evaluator::CachedStationLookup`] -- shared lookup state for priority scoring
//! - [`cqdx_bridge::CqdxBridge`] -- background task keeping the cqdx.io cache fresh
//!
//! ## Crate Relationships
//! - Receives from: `pancetta-ft8` (decoded messages), `pancetta-qso` (TX decisions),
//!   `pancetta-hamlib` (rig state), `pancetta-cqdx` (spots/rarity)
//! - Sends to: `pancetta-ft8` (TX encode), `pancetta-hamlib` (PTT/freq commands),
//!   `pancetta-tui` (display updates), `pancetta-qso` (decoded messages)

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md
#![allow(dead_code, unused_imports)]

pub mod coordinator;
pub mod cqdx_bridge;
pub mod message_bus;
pub mod priority_evaluator;
pub mod runtime;
