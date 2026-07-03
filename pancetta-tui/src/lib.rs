//! # pancetta-tui
//!
//! Terminal UI (ratatui) — 4 activity views (Operate/Hunt/Run/Monitor), the
//! TX-placement instrument, decoded messages, QSO state, band activity.
//!
//! Terminal User Interface library for the Pancetta amateur radio application.
//! Provides real-time display of FT8 decodes, band activity, station
//! information, and — via `v`/`V` — a per-workflow layout (`view::ActiveView`):
//! Operate (default), Hunt (DX-hunting), Run (pileup), and Monitor (the
//! original waterfall-centric big-picture view). Operate/Hunt/Run replace the
//! waterfall with a vacancy-first TX-placement instrument (band openness +
//! ranked candidate frequencies + park interactions); Monitor keeps the
//! waterfall. A single global "focus" (`App::focused_callsign`) is shared
//! across every panel, keyboard and mouse alike.
//!
//! ## Data Flow
//! `pancetta` coordinator (decoded messages, waterfall, placement snapshots, QSO state) -> **pancetta-tui** -> terminal display
//!
//! User keyboard/mouse input -> **pancetta-tui** -> `pancetta` coordinator (commands)
//!
//! ## Key Types
//! - [`App`] -- root application state driving all UI panels
//! - [`tui_runner::TuiRunner`] -- the async event loop: polls crossterm key/mouse
//!   events and the coordinator's `TuiMessage` channel, and renders each frame
//! - [`DecodedMessageView`] -- view model for a single decoded FT8 message
//! - [`QsoStatus`] -- current QSO state for display (calling, in-progress, complete)
//! - [`view::ActiveView`] -- which of the 4 activity views is showing
//!
//! ## Crate Relationships
//! - Receives from: `pancetta` coordinator (live decode stream, TX-placement
//!   snapshots, QSO state) — `pancetta-tui` never depends on `pancetta-qso`;
//!   the coordinator's relay converts qso-crate types into TUI-local ones.
//! - Sends to: `pancetta` coordinator (user commands: start CQ, set frequency, etc.)

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md
#![allow(dead_code, unused_imports)]

pub mod app;
pub mod config;
pub mod dxcc;
mod dxcc_table;
pub mod tui_runner;
pub mod ui;
pub mod view;
pub mod widgets;

// Re-export main types for convenience
pub use app::{
    ActivePanel, App, AutonomousStatus, ColorCapability, DecodedMessageView, DevicePanel,
    DeviceSelectionState, DxStation, PipelineHealth, QsoStatus, StationInfo,
};
pub use config::{Config, Theme};
pub use view::ActiveView;

/// TUI library version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
