//! # Pancetta TUI Library
//!
//! Terminal User Interface library for the Pancetta amateur radio application.
//! Provides real-time display of FT8 decodes, band activity, and station information.

pub mod app;
pub mod config;
pub mod events;
pub mod tui_runner;
pub mod ui;
pub mod widgets;

// Re-export main types for convenience
pub use app::{
    ActivePanel, App, AutonomousStatus, DecodedMessage, DevicePanel, DeviceSelectionState,
    DxStation, QsoStatus, StationInfo,
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

/// Run the TUI application
pub async fn run_tui(config: config::Config, audio_device: Option<String>) -> Result<()> {
    let _app = create_app(config, audio_device).await?;

    // TODO: Implement TUI main loop
    // This would integrate with the main application coordinator
    // For now, just return success

    Ok(())
}
