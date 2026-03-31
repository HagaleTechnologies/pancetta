//! # Pancetta - High-Performance Amateur Radio FT8 Processing Application
//!
//! The main entry point for the Pancetta application, which integrates:
//! - Real-time audio processing (pancetta-audio)
//! - Digital signal processing pipeline (pancetta-dsp)
//! - FT8 decoder with >95% accuracy (pancetta-ft8)
//! - Interactive terminal user interface (pancetta-tui)
//! - Comprehensive configuration management (pancetta-config)
//!
//! ## Architecture
//!
//! Pancetta uses a message-driven architecture with dedicated components:
//! - **Audio Coordinator**: Manages audio input and real-time processing
//! - **DSP Pipeline**: Processes audio with <1ms latency
//! - **FT8 Decoder**: Decodes 50+ simultaneous FT8 signals
//! - **TUI Manager**: Provides real-time user interface
//! - **Message Bus**: High-performance inter-component communication
//!
//! ## Performance Goals
//!
//! - Audio processing latency: <1ms
//! - FT8 decode accuracy: >95% at -20dB SNR
//! - Simultaneous decodes: 50+
//! - Memory usage: <100MB
//! - CPU usage: <25% on modern hardware

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use futures::StreamExt;
use pancetta_config::Config;
use signal_hook::consts::SIGINT;
use signal_hook_tokio::Signals;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::signal;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

mod logging;

use pancetta_lib::coordinator::ApplicationCoordinator;

/// Pancetta - High-Performance Amateur Radio FT8 Processing Application
#[derive(Clone, Parser)]
#[command(name = "pancetta")]
#[command(version = env!("CARGO_PKG_VERSION"))]
#[command(about = "High-performance amateur radio FT8 processing")]
#[command(long_about = r#"
Pancetta is a high-performance amateur radio application optimized for FT8 digital mode processing.

Features:
- Real-time audio processing with <1ms latency
- FT8 decoder with >95% accuracy at -20dB SNR
- Support for 50+ simultaneous decodes
- Interactive terminal user interface
- Comprehensive configuration management
- Hot-reload configuration support

The application integrates multiple specialized components:
- Audio input and real-time streaming
- Digital signal processing pipeline
- FT8 signal decoding and analysis
- User interface with band activity monitoring
- Configuration management with validation

Performance targets:
- Audio latency: <1ms
- Memory usage: <100MB
- CPU usage: <25% on modern hardware
"#)]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,

    /// Configuration file path
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,

    /// Audio input device name or index
    #[arg(short, long, global = true)]
    audio_device: Option<String>,

    /// Enable debug logging
    #[arg(short, long, global = true)]
    debug: bool,

    /// Enable verbose logging (overrides debug)
    #[arg(short, long, global = true)]
    verbose: bool,

    /// Log output format (text, json)
    #[arg(long, default_value = "text", global = true)]
    log_format: LogFormat,

    /// Disable audio processing (useful for testing)
    #[arg(long, global = true)]
    no_audio: bool,

    /// Disable TUI (run in headless mode)
    #[arg(long, global = true)]
    headless: bool,

    /// WAV file to decode (enables playback mode — decodes and exits)
    #[arg(long, global = true)]
    wav: Option<PathBuf>,

    /// Enable metrics collection
    #[arg(long, global = true)]
    metrics: bool,

    /// Metrics server port (requires --metrics)
    #[arg(long, default_value = "9090", global = true)]
    metrics_port: u16,
}

#[derive(Clone, Subcommand)]
enum Commands {
    /// Run the main application
    Run(RunArgs),
    /// Test audio device configuration
    TestAudio(TestAudioArgs),
    /// Validate configuration files
    Config(ConfigArgs),
    /// Show system information and capabilities
    Info,
    /// Run benchmarks for performance testing
    Benchmark(BenchmarkArgs),
    /// Benchmark decoder against ft8_lib reference
    BenchmarkDecode(BenchmarkDecodeArgs),
}

#[derive(Clone, Args)]
struct RunArgs {
    /// Override station callsign
    #[arg(long)]
    callsign: Option<String>,

    /// Override operating frequency in Hz
    #[arg(long)]
    frequency: Option<f64>,

    /// Override power output in watts
    #[arg(long)]
    power: Option<u32>,
}

#[derive(Clone, Args)]
struct TestAudioArgs {
    /// List available audio devices
    #[arg(short, long)]
    list: bool,

    /// Test specific audio device
    #[arg(short, long)]
    device: Option<String>,

    /// Test duration in seconds
    #[arg(short, long, default_value = "10")]
    duration: u64,
}

#[derive(Clone, Args)]
struct ConfigArgs {
    /// Validate configuration and exit
    #[arg(short, long)]
    validate: bool,

    /// Show current configuration
    #[arg(short, long)]
    show: bool,

    /// Generate default configuration file
    #[arg(short, long)]
    generate: Option<PathBuf>,
}

#[derive(Clone, Args)]
struct BenchmarkDecodeArgs {
    /// Path to a WAV file or directory of WAV files
    #[arg(required = true)]
    path: String,

    /// Output format: "text" or "json"
    #[arg(long, default_value = "text")]
    format: String,
}

#[derive(Clone, Args)]
struct BenchmarkArgs {
    /// Run audio processing benchmarks
    #[arg(long)]
    audio: bool,

    /// Run DSP pipeline benchmarks
    #[arg(long)]
    dsp: bool,

    /// Run FT8 decoder benchmarks
    #[arg(long)]
    ft8: bool,

    /// Run all benchmarks
    #[arg(long)]
    all: bool,

    /// Number of iterations for benchmarks
    #[arg(long, default_value = "100")]
    iterations: usize,
}

#[derive(Clone, Copy, Debug)]
enum LogFormat {
    Text,
    Json,
}

impl std::str::FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "text" => Ok(LogFormat::Text),
            "json" => Ok(LogFormat::Json),
            _ => Err(format!("Invalid log format: {}", s)),
        }
    }
}

impl std::fmt::Display for LogFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogFormat::Text => write!(f, "text"),
            LogFormat::Json => write!(f, "json"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // Initialize logging first
    init_logging(&cli)?;

    info!(
        "Starting Pancetta v{} - High-Performance Amateur Radio FT8 Processing",
        env!("CARGO_PKG_VERSION")
    );

    // Handle subcommands
    if let Some(ref command) = cli.command {
        return handle_command(command.clone(), &cli).await;
    }

    // Run main application
    run_application(cli).await
}

async fn run_application(cli: Cli) -> Result<()> {
    // Load configuration
    let config = load_configuration(&cli).await?;

    info!("Configuration loaded successfully");
    debug!("Configuration: {}", config.summary());

    // Validate configuration
    config
        .validate()
        .context("Configuration validation failed")?;

    // Create shutdown signal handler
    let shutdown = Arc::new(AtomicBool::new(false));
    let shutdown_clone = shutdown.clone();

    // Set up signal handlers
    let shutdown_for_signals = shutdown.clone();
    tokio::spawn(async move {
        match Signals::new(&[SIGINT]) {
            Ok(mut signals) => {
                while let Some(signal) = signals.next().await {
                    match signal {
                        SIGINT => {
                            info!("Received SIGINT, initiating graceful shutdown");
                            shutdown_clone.store(true, Ordering::Release);
                            break;
                        }
                        _ => {}
                    }
                }
            }
            Err(e) => {
                error!("Failed to register signal handler: {}", e);
                shutdown_clone.store(true, Ordering::Release);
            }
        }
    });

    // Alternative signal handler for Windows/cross-platform compatibility
    tokio::spawn(async move {
        if let Err(e) = signal::ctrl_c().await {
            error!("Failed to listen for ctrl+c: {}", e);
        }
        warn!("Received Ctrl+C, initiating graceful shutdown");
        shutdown_for_signals.store(true, Ordering::Release);
    });

    // Create application coordinator
    let coordinator = ApplicationCoordinator::new(
        config,
        cli.audio_device,
        cli.no_audio,
        cli.headless,
        cli.metrics,
        cli.metrics_port,
        cli.wav,
        shutdown.clone(),
    )
    .await?;

    info!("Application coordinator initialized");

    // Start the application
    let result = coordinator.run().await;

    // Handle shutdown
    match result {
        Ok(_) => {
            info!("Application completed successfully");
        }
        Err(e) => {
            error!("Application error: {}", e);
            // Ensure graceful shutdown even on error
            shutdown.store(true, Ordering::Release);
        }
    }

    // Clean shutdown
    info!("Performing cleanup...");
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    info!("Pancetta shutdown complete");

    Ok(())
}

async fn handle_command(command: Commands, cli: &Cli) -> Result<()> {
    match command {
        Commands::Run(_args) => {
            // Run main application with current CLI
            run_application(cli.clone()).await
        }
        Commands::TestAudio(args) => test_audio_command(args).await,
        Commands::Config(args) => config_command(args, cli).await,
        Commands::Info => info_command().await,
        Commands::Benchmark(args) => benchmark_command(args).await,
        Commands::BenchmarkDecode(args) => benchmark_decode_command(args).await,
    }
}

async fn test_audio_command(_args: TestAudioArgs) -> Result<()> {
    eprintln!("Error: audio testing is not yet implemented");
    std::process::exit(1);
}

async fn config_command(args: ConfigArgs, cli: &Cli) -> Result<()> {
    if args.validate {
        let config = load_configuration(cli).await?;
        match config.validate() {
            Ok(_) => {
                println!("Configuration validation: PASS");
                info!("Configuration is valid");
            }
            Err(e) => {
                println!("Configuration validation: FAIL");
                error!("Configuration error: {}", e);
                return Err(e.into());
            }
        }
        return Ok(());
    }

    if args.show {
        let config = load_configuration(cli).await?;
        println!("{}", config.summary());
        return Ok(());
    }

    if let Some(output_path) = args.generate {
        let default_config = Config::default();
        default_config.save_to_file(&output_path)?;
        println!("Generated default configuration: {}", output_path.display());
        info!("Default configuration saved to: {}", output_path.display());
        return Ok(());
    }

    println!("Use --help for config command options");
    Ok(())
}

async fn info_command() -> Result<()> {
    println!("Pancetta System Information");
    println!("===========================");
    println!("Version: {}", env!("CARGO_PKG_VERSION"));
    println!();

    // System information
    println!("System:");
    println!("  OS: {}", std::env::consts::OS);
    println!("  Architecture: {}", std::env::consts::ARCH);
    println!("  CPU cores: {}", num_cpus::get());
    println!();

    // Component versions
    println!("Components:");
    println!("  pancetta-dsp: {}", pancetta_dsp::VERSION);
    println!();

    // Audio devices require the audio subsystem — use `pancetta test-audio --list` when implemented
    println!("Audio devices: (use test-audio --list when implemented)");

    Ok(())
}

async fn benchmark_command(_args: BenchmarkArgs) -> Result<()> {
    eprintln!("Error: benchmarks are not yet implemented");
    std::process::exit(1);
}

async fn benchmark_decode_command(args: BenchmarkDecodeArgs) -> Result<()> {
    use pancetta_ft8::benchmark::{compare_results, decode_wav_to_results, BenchmarkResult};
    use std::path::Path;

    let path = Path::new(&args.path);

    // Collect WAV files to process
    let wav_files: Vec<String> = if path.is_dir() {
        let mut files: Vec<String> = std::fs::read_dir(path)
            .with_context(|| format!("Cannot read directory: {}", args.path))?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) == Some("wav") {
                    p.to_str().map(|s| s.to_string())
                } else {
                    None
                }
            })
            .collect();
        files.sort();
        files
    } else {
        vec![args.path.clone()]
    };

    if wav_files.is_empty() {
        eprintln!("No WAV files found at: {}", args.path);
        std::process::exit(1);
    }

    // Decode each file
    let mut results: Vec<BenchmarkResult> = Vec::new();
    for wav_path in &wav_files {
        eprint!("Decoding {} ...", wav_path);
        match decode_wav_to_results(wav_path) {
            Ok(result) => {
                eprintln!(
                    " pancetta={} ft8lib={} ({:.0}ms)",
                    result.pancetta_decodes.len(),
                    result.ft8lib_decodes.len(),
                    result.processing_time_ms
                );
                results.push(result);
            }
            Err(e) => {
                eprintln!(" ERROR: {}", e);
            }
        }
    }

    // Aggregate and report
    let summary = compare_results(&results);

    match args.format.as_str() {
        "json" => {
            println!("{}", serde_json::to_string_pretty(&summary)?);
        }
        _ => {
            println!();
            println!("=== Decoder Benchmark Summary ===");
            println!("Files processed : {}", summary.total_files);
            println!("Pancetta decodes: {}", summary.pancetta_total);
            println!("ft8_lib decodes : {}", summary.ft8lib_total);
            println!("Both decoded    : {}", summary.both_decoded);
            println!("Pancetta only   : {}", summary.pancetta_only);
            println!("ft8_lib only    : {}", summary.ft8lib_only);
            println!("Parity          : {:.1}%", summary.parity_percent);

            if !summary.per_file.is_empty() {
                println!();
                println!("Per-file breakdown:");
                for r in &summary.per_file {
                    println!(
                        "  {} — pancetta={} ft8lib={} ({:.0}ms)",
                        r.file_path,
                        r.pancetta_decodes.len(),
                        r.ft8lib_decodes.len(),
                        r.processing_time_ms
                    );
                }
            }
        }
    }

    Ok(())
}

async fn load_configuration(cli: &Cli) -> Result<Config> {
    let mut config = if let Some(config_path) = &cli.config {
        Config::load_from_file(config_path)
            .with_context(|| format!("Failed to load config from {}", config_path.display()))?
    } else {
        Config::load_default().with_context(|| "Failed to load default configuration")?
    };

    // First-run setup: if callsign is still the default, prompt the user
    if config.station.callsign == "N0CALL" && !cli.headless && cli.wav.is_none() {
        if let Some(updated) = run_first_time_setup(&config)? {
            config = updated;
        }
    }

    Ok(config)
}

/// Interactive first-run setup wizard.
/// Prompts for callsign, grid square, and saves the config file.
fn run_first_time_setup(config: &Config) -> Result<Option<Config>> {
    use std::io::{self, Write};

    println!();
    println!("=== Pancetta First-Run Setup ===");
    println!();
    println!("No station configuration found. Let's set up the basics.");
    println!("(Press Enter to skip any field and use the default.)");
    println!();

    // Callsign
    print!("Your callsign [N0CALL]: ");
    io::stdout().flush()?;
    let mut callsign = String::new();
    io::stdin().read_line(&mut callsign)?;
    let callsign = callsign.trim().to_uppercase();
    let callsign = if callsign.is_empty() {
        "N0CALL".to_string()
    } else {
        callsign
    };

    // Grid square
    print!(
        "Your Maidenhead grid square [{}]: ",
        config.station.grid_square
    );
    io::stdout().flush()?;
    let mut grid = String::new();
    io::stdin().read_line(&mut grid)?;
    let grid = grid.trim().to_string();
    let grid = if grid.is_empty() {
        config.station.grid_square.clone()
    } else {
        grid
    };

    // Power
    print!("TX power in watts [{}]: ", config.station.power_watts);
    io::stdout().flush()?;
    let mut power_str = String::new();
    io::stdin().read_line(&mut power_str)?;
    let power: u32 = power_str
        .trim()
        .parse()
        .unwrap_or(config.station.power_watts);

    let mut new_config = config.clone();
    new_config.station.callsign = callsign.clone();
    new_config.station.grid_square = grid.clone();
    new_config.station.power_watts = power;

    // Save config
    let config_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pancetta");
    let config_path = config_dir.join("pancetta.toml");

    print!("\nSave configuration to {}? [Y/n]: ", config_path.display());
    io::stdout().flush()?;
    let mut save_choice = String::new();
    io::stdin().read_line(&mut save_choice)?;
    let save = save_choice.trim().is_empty() || save_choice.trim().to_lowercase().starts_with('y');

    if save {
        std::fs::create_dir_all(&config_dir)?;
        new_config
            .save_to_file(&config_path)
            .with_context(|| format!("Failed to save config to {}", config_path.display()))?;
        println!("Configuration saved to {}", config_path.display());
    }

    println!();
    println!("Station: {} / {} / {}W", callsign, grid, power);
    println!("Setup complete! Starting Pancetta...");
    println!();

    Ok(Some(new_config))
}

fn init_logging(cli: &Cli) -> Result<()> {
    let log_level = if cli.verbose {
        "trace"
    } else if cli.debug {
        "debug"
    } else {
        "info"
    };

    let env_filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new(format!("pancetta={},warn", log_level)));

    // Set up file logging with daily rotation to ~/.pancetta/logs/
    let log_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pancetta")
        .join("logs");

    // Create log directory (ignore errors — file logging is best-effort)
    let _ = std::fs::create_dir_all(&log_dir);

    let file_appender = tracing_appender::rolling::daily(&log_dir, "pancetta.log");
    let (non_blocking, _guard) = tracing_appender::non_blocking(file_appender);

    // Keep the guard alive for the process lifetime
    std::mem::forget(_guard);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true);

    // Console layer — always text format for now (JSON would need separate branch)
    let console_layer = tracing_subscriber::fmt::layer()
        .with_target(false)
        .with_thread_ids(true)
        .with_file(cli.debug || cli.verbose)
        .with_line_number(cli.debug || cli.verbose);

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_cmd::Command;
    use predicates::prelude::*;

    #[test]
    fn test_cli_help() {
        let mut cmd = Command::cargo_bin("pancetta").unwrap();
        cmd.arg("--help");
        cmd.assert()
            .success()
            .stdout(predicate::str::contains("high-performance amateur radio"));
    }

    #[test]
    fn test_cli_version() {
        let mut cmd = Command::cargo_bin("pancetta").unwrap();
        cmd.arg("--version");
        cmd.assert()
            .success()
            .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
    }

    #[tokio::test]
    async fn test_config_validation() {
        let config = Config::default();
        assert!(config.validate().is_ok());
    }

    #[test]
    fn test_log_format_parsing() {
        assert!(matches!(
            "text".parse::<LogFormat>().unwrap(),
            LogFormat::Text
        ));
        assert!(matches!(
            "json".parse::<LogFormat>().unwrap(),
            LogFormat::Json
        ));
        assert!("invalid".parse::<LogFormat>().is_err());
    }
}
