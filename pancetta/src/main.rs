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

#![allow(dead_code, unused_imports)]

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use pancetta_config::Config;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::signal;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

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
    /// Interactive setup wizard for station, audio, rig, and PTT
    Setup,
    /// Test rig connection (serial port, CAT, PTT)
    TestRig(TestRigArgs),
}

#[derive(Clone, Args)]
struct TestRigArgs {
    /// Test PTT by keying TX for 1 second (use with caution!)
    #[arg(long)]
    ptt: bool,
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
    let _log_guard = init_logging(&cli, cli.headless)?;

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

    // Set up Ctrl+C signal handler
    let shutdown_for_signals = shutdown.clone();
    tokio::spawn(async move {
        if let Err(e) = signal::ctrl_c().await {
            error!("Failed to listen for ctrl+c: {}", e);
        }
        info!("Received Ctrl+C, initiating graceful shutdown");
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
        Commands::Setup => setup_command().await,
        Commands::TestRig(args) => test_rig_command(args, cli).await,
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
    println!();
    println!("=== Pancetta First-Run Setup ===");
    println!();
    println!("No station configuration found. Let's set up the basics.");
    println!("(Press Enter to skip any field and use the default.)");
    println!();

    let mut new_config = config.clone();
    setup_station(&mut new_config)?;

    let config_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pancetta");
    let config_path = config_dir.join("pancetta.toml");

    if prompt_yes_no(&format!("Save configuration to {}?", config_path.display()), true)? {
        std::fs::create_dir_all(&config_dir)?;
        new_config
            .save_to_file(&config_path)
            .with_context(|| format!("Failed to save config to {}", config_path.display()))?;
        println!("Configuration saved to {}", config_path.display());
    }

    println!();
    println!(
        "Station: {} / {} / {}W",
        new_config.station.callsign, new_config.station.grid_square, new_config.station.power_watts
    );
    println!("Setup complete! Starting Pancetta...");
    println!();

    Ok(Some(new_config))
}

// ---------------------------------------------------------------------------
// Setup wizard helpers
// ---------------------------------------------------------------------------

fn prompt_line(prompt: &str) -> Result<String> {
    use std::io::{self, Write};
    print!("{}", prompt);
    io::stdout().flush()?;
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    Ok(input.trim().to_string())
}

fn prompt_yes_no(prompt: &str, default_yes: bool) -> Result<bool> {
    let hint = if default_yes { "[Y/n]" } else { "[y/N]" };
    let input = prompt_line(&format!("{} {}: ", prompt, hint))?;
    if input.is_empty() {
        return Ok(default_yes);
    }
    Ok(input.to_lowercase().starts_with('y'))
}

fn prompt_choice(prompt: &str, max: usize) -> Result<Option<usize>> {
    let input = prompt_line(prompt)?;
    if input.is_empty() {
        return Ok(None);
    }
    match input.parse::<usize>() {
        Ok(n) if n >= 1 && n <= max => Ok(Some(n)),
        _ => {
            println!("  Invalid choice, keeping current setting.");
            Ok(None)
        }
    }
}

fn setup_station(config: &mut Config) -> Result<()> {
    println!("--- Station ---");
    println!();

    let input = prompt_line(&format!("  Callsign [{}]: ", config.station.callsign))?;
    if !input.is_empty() {
        config.station.callsign = input.to_uppercase();
    }

    let input = prompt_line(&format!("  Grid square [{}]: ", config.station.grid_square))?;
    if !input.is_empty() {
        config.station.grid_square = input;
    }

    let input = prompt_line(&format!("  TX power watts [{}]: ", config.station.power_watts))?;
    if !input.is_empty() {
        if let Ok(p) = input.parse::<u32>() {
            config.station.power_watts = p;
        } else {
            println!("  Invalid number, keeping {}W.", config.station.power_watts);
        }
    }

    println!();
    Ok(())
}

fn setup_audio(config: &mut Config) -> Result<()> {
    println!("--- Audio Devices ---");
    println!();

    match pancetta_audio::device::AudioDeviceManager::new() {
        Ok(mgr) => {
            let devices = mgr.list_devices();

            // Input devices
            let inputs: Vec<_> = devices
                .iter()
                .filter(|(_, info)| info.supports_input)
                .collect();
            if inputs.is_empty() {
                println!("  No input devices found.");
            } else {
                println!("  Input devices:");
                for (i, (_, info)) in inputs.iter().enumerate() {
                    let marker = if info.is_default_input { " (system default)" } else { "" };
                    println!("    [{}] {}{}", i + 1, info.name, marker);
                }
                let current = &config.audio.input_device;
                if let Some(choice) = prompt_choice(
                    &format!("  Select input device [current: {}]: ", current),
                    inputs.len(),
                )? {
                    config.audio.input_device = inputs[choice - 1].1.name.clone();
                }
            }
            println!();

            // Output devices
            let outputs: Vec<_> = devices
                .iter()
                .filter(|(_, info)| info.supports_output)
                .collect();
            if outputs.is_empty() {
                println!("  No output devices found.");
            } else {
                println!("  Output devices:");
                for (i, (_, info)) in outputs.iter().enumerate() {
                    let marker = if info.is_default_output { " (system default)" } else { "" };
                    println!("    [{}] {}{}", i + 1, info.name, marker);
                }
                let current = &config.audio.output_device;
                if let Some(choice) = prompt_choice(
                    &format!("  Select output device [current: {}]: ", current),
                    outputs.len(),
                )? {
                    config.audio.output_device = outputs[choice - 1].1.name.clone();
                }
            }
        }
        Err(e) => {
            println!("  Could not enumerate audio devices: {}", e);
            println!("  You can manually enter device names.");
            let input = prompt_line(&format!(
                "  Input device [{}]: ",
                config.audio.input_device
            ))?;
            if !input.is_empty() {
                config.audio.input_device = input;
            }
            let input = prompt_line(&format!(
                "  Output device [{}]: ",
                config.audio.output_device
            ))?;
            if !input.is_empty() {
                config.audio.output_device = input;
            }
        }
    }

    println!();
    Ok(())
}

fn setup_rig(config: &mut Config) -> Result<()> {
    println!("--- Rig Control ---");
    println!();

    let currently_enabled = config.rig.interface.enabled;
    if !prompt_yes_no("  Enable rig control?", currently_enabled)? {
        config.rig.interface.enabled = false;
        println!("  Rig control disabled.");
        println!();
        return Ok(());
    }
    config.rig.interface.enabled = true;

    // Rig model
    let input = prompt_line(&format!("  Rig model [{}]: ", config.rig.model))?;
    if !input.is_empty() {
        config.rig.model = input;
    }

    // Serial port
    println!();
    match serialport::available_ports() {
        Ok(ports) if !ports.is_empty() => {
            println!("  Available serial ports:");
            for (i, port) in ports.iter().enumerate() {
                let detail = match &port.port_type {
                    serialport::SerialPortType::UsbPort(usb) => {
                        let product = usb.product.as_deref().unwrap_or("Unknown");
                        let mfg = usb.manufacturer.as_deref().unwrap_or("");
                        if mfg.is_empty() {
                            product.to_string()
                        } else {
                            format!("{} ({})", product, mfg)
                        }
                    }
                    serialport::SerialPortType::BluetoothPort => "Bluetooth".to_string(),
                    serialport::SerialPortType::PciPort => "PCI".to_string(),
                    _ => String::new(),
                };
                if detail.is_empty() {
                    println!("    [{}] {}", i + 1, port.port_name);
                } else {
                    println!("    [{}] {} — {}", i + 1, port.port_name, detail);
                }
            }
            if let Some(choice) = prompt_choice(
                &format!("  Select serial port [current: {}]: ", config.rig.interface.port),
                ports.len(),
            )? {
                config.rig.interface.port = ports[choice - 1].port_name.clone();
            }
        }
        _ => {
            println!("  No serial ports detected (or enumeration failed).");
            let input = prompt_line(&format!(
                "  Serial port path [{}]: ",
                config.rig.interface.port
            ))?;
            if !input.is_empty() {
                config.rig.interface.port = input;
            }
        }
    }

    // Baud rate
    println!();
    let baud_rates = [4800u32, 9600, 19200, 38400, 57600, 115200];
    println!("  Baud rates:");
    for (i, rate) in baud_rates.iter().enumerate() {
        let marker = if *rate == config.rig.interface.baud_rate {
            " (current)"
        } else {
            ""
        };
        println!("    [{}] {}{}", i + 1, rate, marker);
    }
    if let Some(choice) = prompt_choice("  Select baud rate: ", baud_rates.len())? {
        config.rig.interface.baud_rate = baud_rates[choice - 1];
    }

    println!();
    Ok(())
}

fn setup_ptt(config: &mut Config) -> Result<()> {
    use pancetta_config::rig::PttMethod;

    println!("--- PTT Control ---");
    println!();

    let methods = [
        (PttMethod::None, "None (no PTT control)"),
        (PttMethod::Cat, "CAT (via rig control)"),
        (PttMethod::Serial, "Serial (RTS/DTR)"),
        (PttMethod::Vox, "VOX (voice-operated)"),
    ];

    for (i, (_, desc)) in methods.iter().enumerate() {
        println!("    [{}] {}", i + 1, desc);
    }

    let current = format!("{:?}", config.rig.ptt.method);
    if let Some(choice) = prompt_choice(
        &format!("  Select PTT method [current: {}]: ", current),
        methods.len(),
    )? {
        config.rig.ptt.method = methods[choice - 1].0.clone();
    }

    println!();
    Ok(())
}

fn setup_frequency(config: &mut Config) -> Result<()> {
    println!("--- Frequency Control ---");
    println!();

    config.rig.frequency.control_enabled =
        prompt_yes_no("  Enable frequency control?", config.rig.frequency.control_enabled)?;

    if config.rig.frequency.control_enabled {
        config.rig.frequency.follow_rig =
            prompt_yes_no("  Follow rig frequency?", config.rig.frequency.follow_rig)?;
    }

    println!();
    Ok(())
}

async fn setup_command() -> Result<()> {
    println!();
    println!("=== Pancetta Setup Wizard ===");
    println!("Press Enter to keep the current value for any field.");
    println!();

    // Load existing config or defaults
    let config_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pancetta");
    let config_path = config_dir.join("pancetta.toml");
    let mut config = if config_path.exists() {
        Config::load_from_file(&config_path).unwrap_or_default()
    } else {
        Config::default()
    };

    setup_station(&mut config)?;
    setup_audio(&mut config)?;
    setup_rig(&mut config)?;
    setup_ptt(&mut config)?;
    setup_frequency(&mut config)?;

    // Summary
    println!("=== Summary ===");
    println!("  Station:   {} / {} / {}W", config.station.callsign, config.station.grid_square, config.station.power_watts);
    println!("  Audio in:  {}", config.audio.input_device);
    println!("  Audio out: {}", config.audio.output_device);
    if config.rig.interface.enabled {
        println!("  Rig:       {} on {} @ {}", config.rig.model, config.rig.interface.port, config.rig.interface.baud_rate);
        println!("  PTT:       {:?}", config.rig.ptt.method);
        println!("  Freq ctrl: {}", if config.rig.frequency.control_enabled { "enabled" } else { "disabled" });
    } else {
        println!("  Rig:       disabled");
    }
    println!();

    if prompt_yes_no(&format!("Save to {}?", config_path.display()), true)? {
        std::fs::create_dir_all(&config_dir)?;
        config
            .save_to_file(&config_path)
            .with_context(|| format!("Failed to save config to {}", config_path.display()))?;
        println!("Configuration saved.");
    }

    println!();
    Ok(())
}

async fn test_rig_command(args: TestRigArgs, cli: &Cli) -> Result<()> {
    use std::time::Duration;

    println!();
    println!("=== Pancetta Rig Test ===");
    println!();

    // Load config to get rig settings
    let config = load_configuration(cli).await?;

    if !config.rig.interface.enabled {
        println!("Rig control is disabled in configuration.");
        println!("Run 'pancetta setup' to configure your rig, or set rig.interface.enabled = true");
        return Ok(());
    }

    let port_name = &config.rig.interface.port;
    let baud_rate = config.rig.interface.baud_rate;

    println!("Rig model:  {}", config.rig.model);
    println!("Port:       {}", port_name);
    println!("Baud rate:  {}", baud_rate);
    println!("PTT method: {:?}", config.rig.ptt.method);
    println!();

    // Step 1: Check serial port exists
    print!("[1/4] Checking serial port... ");
    match serialport::available_ports() {
        Ok(ports) => {
            let found = ports.iter().any(|p| p.port_name == *port_name);
            if found {
                println!("FOUND");
            } else {
                println!("NOT FOUND");
                println!();
                println!("  Available ports:");
                if ports.is_empty() {
                    println!("    (none detected)");
                } else {
                    for p in &ports {
                        println!("    {}", p.port_name);
                    }
                }
                println!();
                println!("  Check your USB cable and run 'pancetta setup' to select the right port.");
                return Ok(());
            }
        }
        Err(e) => {
            println!("ERROR ({})", e);
            return Ok(());
        }
    }

    // Step 2: Open serial port
    print!("[2/4] Opening serial port... ");
    let port = serialport::new(port_name, baud_rate)
        .timeout(Duration::from_secs(2))
        .open();

    let mut port = match port {
        Ok(p) => {
            println!("OK");
            p
        }
        Err(e) => {
            println!("FAILED");
            println!();
            match e.kind() {
                serialport::ErrorKind::Io(std::io::ErrorKind::PermissionDenied) => {
                    println!("  Permission denied. You may need to add your user to the 'dialout' group");
                    println!("  or check device permissions on {}.", port_name);
                }
                serialport::ErrorKind::Io(std::io::ErrorKind::NotFound) => {
                    println!("  Device not found. The rig may be powered off or USB cable disconnected.");
                }
                _ => {
                    println!("  Error: {}", e);
                }
            }
            return Ok(());
        }
    };

    // Step 3: Try reading from port (check if rig is sending data)
    print!("[3/4] Listening for rig data (2s)... ");
    let mut buf = vec![0u8; 256];
    match port.read(&mut buf) {
        Ok(n) => {
            println!("OK ({} bytes received)", n);
            // Show first few bytes as hex for debugging
            let hex: Vec<String> = buf[..n.min(16)].iter().map(|b| format!("{:02X}", b)).collect();
            println!("       Data: {}", hex.join(" "));
        }
        Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
            println!("OK (no unsolicited data — normal for most rigs)");
        }
        Err(e) => {
            println!("ERROR ({})", e);
        }
    }

    // Step 4: PTT test (only if requested)
    if args.ptt {
        use pancetta_config::rig::PttMethod;

        println!("[4/4] Testing PTT...");
        match config.rig.ptt.method {
            PttMethod::None => {
                println!("       PTT method is 'none' — skipping. Configure PTT in 'pancetta setup'.");
            }
            PttMethod::Serial => {
                println!("       Asserting RTS for 1 second...");
                if let Err(e) = port.write_request_to_send(true) {
                    println!("       RTS ON failed: {}", e);
                } else {
                    println!("       RTS ON — check your rig's TX indicator");
                    std::thread::sleep(Duration::from_secs(1));
                    let _ = port.write_request_to_send(false);
                    println!("       RTS OFF");
                }
            }
            PttMethod::Cat => {
                println!("       CAT PTT requires hamlib — not yet implemented in test mode.");
                println!("       Serial port connectivity looks good though.");
            }
            PttMethod::Vox => {
                println!("       VOX is audio-triggered — no serial test needed.");
                println!("       VOX will activate when audio is sent to the rig.");
            }
            other => {
                println!("       PTT method {:?} not supported in test mode.", other);
            }
        }
    } else {
        println!("[4/4] PTT test: skipped (use --ptt to test)");
    }

    println!();
    println!("Rig test complete.");
    Ok(())
}

fn init_logging(cli: &Cli, headless: bool) -> Result<tracing_appender::non_blocking::WorkerGuard> {
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
    let (non_blocking, guard) = tracing_appender::non_blocking(file_appender);

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false)
        .with_target(true)
        .with_thread_ids(true);

    // Console layer — only when running headless (TUI owns stdout otherwise)
    let console_layer = if headless {
        Some(
            tracing_subscriber::fmt::layer()
                .with_target(false)
                .with_thread_ids(true)
                .with_file(cli.debug || cli.verbose)
                .with_line_number(cli.debug || cli.verbose),
        )
    } else {
        None
    };

    tracing_subscriber::registry()
        .with(env_filter)
        .with(console_layer)
        .with(file_layer)
        .init();

    Ok(guard)
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
