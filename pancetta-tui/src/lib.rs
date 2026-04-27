//! # pancetta-tui
//!
//! Terminal UI (ratatui) — waterfall, decoded messages, QSO state, band activity.
//!
//! Terminal User Interface library for the Pancetta amateur radio application.
//! Provides real-time display of FT8 decodes, band activity, and station information.
//!
//! ## Data Flow
//! `pancetta` coordinator (decoded messages, waterfall, QSO state) -> **pancetta-tui** -> terminal display
//!
//! User keyboard input -> **pancetta-tui** -> `pancetta` coordinator (commands)
//!
//! ## Key Types
//! - [`App`] -- root application state driving all UI panels
//! - [`Event`] -- input events from keyboard and coordinator messages
//! - [`EventHandler`] -- async event loop polling keyboard and message channels
//! - [`DecodedMessageView`] -- view model for a single decoded FT8 message
//! - [`QsoStatus`] -- current QSO state for display (calling, in-progress, complete)
//!
//! ## Crate Relationships
//! - Receives from: `pancetta` coordinator (live decode stream, QSO state)
//! - Sends to: `pancetta` coordinator (user commands: start CQ, set frequency, etc.)

#![allow(missing_docs)] // TODO: documentation pass pending — see CONTRIBUTING.md
#![allow(dead_code, unused_imports)]

pub mod app;
pub mod config;
pub mod events;
pub mod tui_runner;
pub mod ui;
pub mod widgets;

// Re-export main types for convenience
pub use app::{
    ActivePanel, App, AutonomousStatus, ColorCapability, DecodedMessageView, DevicePanel,
    DeviceSelectionState, DxStation, PipelineHealth, QsoStatus, StationInfo,
};
pub use config::{Config, Theme};
pub use events::{Event, EventHandler};

use anyhow::Result;

/// TUI library version
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Create a new TUI application
pub async fn create_app(config: config::Config, audio_device: Option<String>) -> Result<App> {
    App::new(config, audio_device).await
}
