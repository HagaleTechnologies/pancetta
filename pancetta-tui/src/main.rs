use anyhow::Result;
use clap::Parser;
use crossterm::{
    event::{DisableMouseCapture, EnableMouseCapture},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{backend::CrosstermBackend, Terminal};
use std::io;
use tracing::{info, Level};
use tracing_subscriber;

mod app;
mod config;
mod events;
mod ui;
mod widgets;

use app::App;
use config::Config;
use events::{Event, EventHandler};

#[derive(Parser)]
#[command(name = "pancetta-tui")]
#[command(about = "Terminal User Interface for Pancetta Ham Radio Digital Mode Monitor")]
#[command(version)]
struct Cli {
    /// Configuration file path
    #[arg(short, long, default_value = "~/.config/pancetta/tui.toml")]
    config: String,

    /// Audio device to use for monitoring
    #[arg(short, long)]
    device: Option<String>,

    /// Enable debug logging
    #[arg(short, long)]
    debug: bool,

    /// Log file path
    #[arg(short, long, default_value = "/tmp/pancetta-tui.log")]
    log_file: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging
    let log_level = if cli.debug { Level::DEBUG } else { Level::INFO };

    let file_appender = tracing_appender::rolling::never("/tmp", "pancetta-tui.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    tracing_subscriber::fmt()
        .with_writer(non_blocking)
        .with_max_level(log_level)
        .init();

    info!("Starting Pancetta TUI v{}", env!("CARGO_PKG_VERSION"));

    // Load configuration
    let config = Config::load(&cli.config).unwrap_or_else(|e| {
        eprintln!("Failed to load config: {}. Using defaults.", e);
        Config::default()
    });

    // Setup terminal
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture)?;

    // Install panic hook so the terminal is restored even on panic
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(std::io::stderr(), crossterm::terminal::LeaveAlternateScreen);
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    // Create app and event handler
    let mut app = App::new(config, cli.device).await?;
    let mut event_handler = EventHandler::new(250); // 250ms tick rate

    info!("Pancetta TUI initialized successfully");

    // Main application loop
    let result = run_app(&mut terminal, &mut app, &mut event_handler).await;

    // Cleanup terminal
    disable_raw_mode()?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )?;
    terminal.show_cursor()?;

    if let Err(err) = result {
        eprintln!("Application error: {}", err);
    }

    info!("Pancetta TUI shutdown complete");
    Ok(())
}

async fn run_app(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
    event_handler: &mut EventHandler,
) -> Result<()> {
    loop {
        // Draw the UI
        terminal.draw(|f| {
            if let Err(e) = ui::draw(f, app) {
                eprintln!("UI rendering error: {}", e);
            }
        })?;

        // Handle events
        match event_handler.next().await {
            Event::Tick => {
                app.tick().await?;
            }
            Event::Key(key_event) => {
                if app.handle_key_event(key_event).await? {
                    break; // Exit requested
                }
            }
            Event::Mouse(mouse_event) => {
                app.handle_mouse_event(mouse_event).await?;
            }
            Event::Resize(width, height) => {
                app.handle_resize(width, height).await?;
            }
            Event::AudioData(data) => {
                app.handle_audio_data(data).await?;
            }
            Event::DecodedMessage(message) => {
                app.handle_decoded_message(message).await?;
            }
        }
    }

    Ok(())
}
