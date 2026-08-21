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
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::signal;
use tracing::{debug, error, info, warn};
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

use pancetta_lib::coordinator::ApplicationCoordinator;

mod doctor;

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

    /// Directory of sequential WAV captures to replay through the full
    /// pipeline (audio → DSP → FT8 → QSO → TUI) at real-time cadence, as if
    /// it were live audio. Files are read in filename order. Exits on its
    /// own a few seconds after the last file is exhausted. Unlike --wav,
    /// this runs the complete pipeline (TUI, QSO engine, priority scoring),
    /// not just the decoder -- intended for demos and scripted recordings.
    #[arg(long, global = true)]
    replay: Option<PathBuf>,

    /// Enable metrics collection
    #[arg(long, global = true)]
    metrics: bool,

    /// Metrics server port (requires --metrics)
    #[arg(long, default_value = "9090", global = true)]
    metrics_port: u16,

    /// Inject a single TransmitRequest after startup, then shutdown when it
    /// completes. For coordinator TX validation. Example:
    ///   pancetta --headless --test-tx "N0CALL N0CALL 73"
    #[arg(long, global = true)]
    test_tx: Option<String>,

    /// Audio frequency offset (Hz) for --test-tx
    #[arg(long, default_value_t = 1500.0, global = true)]
    test_tx_offset: f64,
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
    /// Check station health: config, clock, audio, decoder, rig — with a
    /// printed fix for every failure. Run this before your first session.
    Doctor,
    /// Test rig connection (serial port, CAT, PTT)
    TestRig(TestRigArgs),
    /// Export logged QSOs to ADIF
    Export(ExportArgs),
    /// Pair this station's agent with cqdx (remote-rig-control device enrollment)
    Pair(PairArgs),
    /// Show this station's agent identity and fingerprint words, for TOFU
    /// comparison against a client (e.g. panino) — read-only, no network
    /// calls. Safe to re-run anytime, including before ever pairing.
    Identity(IdentityArgs),
}

#[derive(Clone, Args)]
struct IdentityArgs {
    /// Number of fingerprint words to print (default: 12, the ceremony's
    /// standard count).
    #[arg(long, default_value = "12")]
    words: usize,
}

#[derive(Clone, Args)]
struct TestRigArgs {
    /// Test PTT by keying TX for 1 second (use with caution!)
    #[arg(long)]
    ptt: bool,
}

#[derive(Clone, Args)]
struct PairArgs {
    /// Single-use pairing code from the cqdx web UI
    #[arg(required = true)]
    code: String,

    /// Human-readable name for this agent shown in cqdx's device list
    #[arg(long)]
    name: Option<String>,

    /// Platform identifier sent during enrollment (default: this OS)
    #[arg(long)]
    platform: Option<String>,

    /// Override the pairing API base URL (default: config's
    /// network.station_agent.pairing_api_url, e.g. "https://cqdx.io/api/v1")
    #[arg(long)]
    pairing_api_url: Option<String>,

    /// Re-pair even if this station already has a paired identity (overwrites
    /// the existing paired.json)
    #[arg(long)]
    force: bool,
}

#[derive(Clone, Args)]
struct ExportArgs {
    /// Output file path for the ADIF export (.adi)
    #[arg(short, long)]
    output: PathBuf,

    /// Override database path (default: ~/.pancetta/qso.db)
    #[arg(long)]
    database: Option<PathBuf>,

    /// Filter by callsign substring (case-insensitive)
    #[arg(long)]
    callsign: Option<String>,
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

    /// Test specific audio device (long flag only — `-d` is taken by global --debug)
    #[arg(long)]
    device: Option<String>,

    /// Test duration in seconds (long flag only)
    #[arg(long, default_value = "10")]
    duration: u64,
}

#[derive(Clone, Args)]
struct ConfigArgs {
    /// Validate configuration and exit
    #[arg(long)]
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

    install_panic_hook();

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
    let (config, config_warnings) = load_configuration_with_warnings(&cli).await?;

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
        cli.replay,
        cli.test_tx,
        cli.test_tx_offset,
        shutdown.clone(),
        config_warnings,
    )
    .await?;

    info!("Application coordinator initialized");

    // Start the application
    let result = coordinator.run().await;

    // Handle shutdown
    match &result {
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

    // Propagate the coordinator's outcome so a genuine application error
    // (e.g. `--replay` pointed at an empty/invalid directory) surfaces as a
    // non-zero exit code instead of being logged and silently discarded.
    result
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
        Commands::Doctor => doctor_command(cli).await,
        Commands::TestRig(args) => test_rig_command(args, cli).await,
        Commands::Export(args) => export_command(args).await,
        Commands::Pair(args) => pair_command(args, cli).await,
        Commands::Identity(args) => identity_command(args, cli).await,
    }
}

/// Print the station's fingerprint words for `agent_key_id` (dispensa
/// Q-0039) — the TOFU-ceremony comparison an operator reads against panino's
/// "Station identity" panel before clicking "Words match — trust this
/// station." Falls back to a plain warning (never panics/aborts the pair
/// flow) if the keyId is somehow malformed, since this is a display nicety
/// layered on top of an already-completed pairing, not load-bearing for it.
fn print_fingerprint_words(agent_key_id: &str, count: usize) {
    match pancetta_agent::fingerprint::fingerprint_words(agent_key_id, count) {
        Ok(words) => {
            println!("Fingerprint words: {}", words.join(" "));
            println!(
                "  Compare these against panino's \"Station identity\" panel before trusting."
            );
        }
        Err(e) => {
            eprintln!("  (could not render fingerprint words: {e})");
        }
    }
}

/// Resolve the agent key directory: explicit config override, else the
/// platform default (`~/.pancetta/agent-keys` or similar — see
/// `default_key_dir`). Shared by `pair` and `identity` so both commands
/// agree on where the station's identity lives.
fn resolve_key_dir(sa_cfg: &pancetta_config::network::StationAgentConfig) -> PathBuf {
    sa_cfg
        .key_dir
        .clone()
        .map(PathBuf::from)
        .unwrap_or_else(pancetta_lib::coordinator::station_agent::default_key_dir)
}

/// Show this station's agent identity and fingerprint words (dispensa
/// Q-0039 follow-up): read-only, no network calls, safe to re-run anytime.
/// Loads (or, on first run, generates + persists) the local `AgentIdentity`
/// — the same identity `pancetta pair` would use — so an operator can check
/// their station's words before ever pairing, or re-check them later
/// without needing a pairing code or `--force`.
async fn identity_command(args: IdentityArgs, cli: &Cli) -> Result<()> {
    use pancetta_agent::keys::AgentIdentity;
    use pancetta_agent::pairing::PairedState;

    let config = load_configuration(cli).await?;
    let sa_cfg = &config.network.station_agent;
    let key_dir = resolve_key_dir(sa_cfg);

    let identity = AgentIdentity::load_or_generate(&key_dir)
        .context("failed to load/generate agent identity")?;
    let key_id = identity.key_id();

    println!();
    println!("=== Pancetta Station Identity ===");
    println!();
    println!("Key dir:   {}", key_dir.display());
    println!("Key ID:    {key_id}");
    print_fingerprint_words(&key_id, args.words);
    println!();

    match PairedState::load(&key_dir) {
        Ok(paired) => {
            println!("Paired: yes ({} pinned IdP key(s)).", paired.idp_keys.len());
        }
        Err(_) => {
            println!("Paired: no — run `pancetta pair <code>` to enroll with cqdx.");
        }
    }
    println!();

    Ok(())
}

/// Run the agent enrollment (pairing) flow against cqdx: `enroll` (POST
/// `/pair/agent`) → PoP-sign the challenge → `complete` (POST
/// `/pair/agent/complete`), then persist the resulting `PairedState` (pinned
/// IdP keys) to the agent's key directory so `station_agent` picks it up on
/// next start. Thin CLI wrapper around already-tested `pancetta_agent`
/// library calls — no new protocol logic here.
async fn pair_command(args: PairArgs, cli: &Cli) -> Result<()> {
    use pancetta_agent::keys::AgentIdentity;
    use pancetta_agent::pairing::{PairedState, PairingClient};
    use pancetta_lib::coordinator::station_agent::net::ReqwestPairingHttp;

    println!();
    println!("=== Pancetta Station-Agent Pairing ===");
    println!();

    let config = load_configuration(cli).await?;
    let sa_cfg = &config.network.station_agent;

    let pairing_api_url = args
        .pairing_api_url
        .clone()
        .or_else(|| sa_cfg.pairing_api_url.clone())
        .filter(|s| !s.is_empty());
    let Some(pairing_api_url) = pairing_api_url else {
        eprintln!("No pairing API URL configured.");
        eprintln!(
            "Set network.station_agent.pairing_api_url in your config (e.g. \
             \"https://cqdx.io/api/v1\"), or pass --pairing-api-url."
        );
        std::process::exit(1);
    };
    let origin = sa_cfg.pairing_origin.clone();

    let key_dir = resolve_key_dir(sa_cfg);

    if !args.force {
        if let Ok(existing) = PairedState::load(&key_dir) {
            eprintln!(
                "This station is already paired (agent_key_id = {}).",
                existing.agent_key_id
            );
            print_fingerprint_words(&existing.agent_key_id, 12);
            eprintln!(
                "Re-run with --force to overwrite {}/paired.json with a new pairing.",
                key_dir.display()
            );
            std::process::exit(1);
        }
    }

    println!("Pairing API:  {pairing_api_url}");
    println!("Key dir:      {}", key_dir.display());
    println!();

    print!("[1/3] Loading (or generating) agent identity... ");
    let identity = AgentIdentity::load_or_generate(&key_dir)
        .context("failed to load/generate agent identity")?;
    let key_id = identity.key_id();
    println!("OK (keyId = {key_id})");

    let platform = args
        .platform
        .clone()
        .unwrap_or_else(|| format!("pancetta-{}", std::env::consts::OS));

    print!("[2/3] Enrolling with cqdx... ");
    let http = ReqwestPairingHttp::new(pairing_api_url, origin);
    let client = PairingClient::new(http, identity);
    let code = args.code.clone();
    let name = args.name.clone();
    // `enroll` synchronously blocks on HTTP via `Handle::block_on` internally
    // (see net.rs's doc) — that's only safe off the async worker threads, so
    // it runs on the blocking-thread pool rather than being awaited directly.
    let enroll_result =
        tokio::task::spawn_blocking(move || client.enroll(&code, name, Some(platform)))
            .await
            .context("pairing task panicked")?;
    let paired = match enroll_result {
        Ok(p) => {
            println!("OK");
            p
        }
        Err(e) => {
            println!("FAILED");
            eprintln!();
            eprintln!("  {e}");
            std::process::exit(1);
        }
    };

    print!("[3/3] Persisting paired state... ");
    paired
        .persist(&key_dir)
        .context("failed to persist paired state")?;
    println!("OK");

    println!();
    println!("Paired. agent_key_id = {}", paired.agent_key_id);
    println!("Pinned {} IdP key(s).", paired.idp_keys.len());
    print_fingerprint_words(&paired.agent_key_id, 12);
    println!();
    println!("Next steps:");
    println!("  1. Set network.station_agent.enabled = true in your config.");
    println!(
        "  2. Add the client's keyId to network.station_agent.tx_allow_list \
         once the client (panino) has paired."
    );
    println!("  3. Restart pancetta to connect to the relay.");
    Ok(())
}

async fn export_command(args: ExportArgs) -> Result<()> {
    use pancetta_qso::adif::AdifProcessor;
    use pancetta_qso::async_database::{QsoDatabase, QsoFilter, QueryOptions};

    let db_path = args.database.unwrap_or_else(|| {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".pancetta")
            .join("qso.db")
    });

    if !db_path.exists() {
        eprintln!("QSO database not found at {}", db_path.display());
        eprintln!("Run pancetta to log some QSOs first.");
        std::process::exit(1);
    }

    println!("Opening database: {}", db_path.display());
    let db = QsoDatabase::open(&db_path)
        .await
        .with_context(|| format!("Failed to open database at {}", db_path.display()))?;

    let filter = QsoFilter {
        callsign_pattern: args.callsign.clone(),
        ..Default::default()
    };
    let options = QueryOptions::default();
    let progresses = db
        .search_qsos(&filter, &options)
        .await
        .context("Failed to search QSOs")?;

    println!("Found {} QSO(s) in database", progresses.len());
    if progresses.is_empty() {
        println!("Nothing to export.");
        return Ok(());
    }

    let processor = AdifProcessor::new();
    let mut adif_file = pancetta_qso::adif::AdifFile {
        header: Default::default(),
        records: Vec::new(),
    };
    for progress in &progresses {
        let adif_qso =
            processor.qso_to_adif(&progress.metadata, progress.metadata.contest_info.as_ref());
        let record = processor.qso_to_record(&adif_qso);
        adif_file.records.push(record);
    }
    let content = processor
        .generate_string(&adif_file)
        .context("Failed to generate ADIF")?;
    tokio::fs::write(&args.output, &content)
        .await
        .with_context(|| format!("Failed to write {}", args.output.display()))?;

    println!(
        "Exported {} QSO(s) to {} ({} bytes)",
        progresses.len(),
        args.output.display(),
        content.len()
    );
    Ok(())
}

async fn test_audio_command(args: TestAudioArgs) -> Result<()> {
    if args.list {
        list_audio_devices();
        return Ok(());
    }
    eprintln!("Error: audio device testing is not yet implemented.");
    eprintln!("       Use `pancetta test-audio --list` to enumerate audio devices.");
    std::process::exit(1);
}

/// Print the available audio input and output devices, flagging the system
/// default in each list. Backs the `pancetta test-audio --list` command and
/// the misconfig hint shown when TX audio is routed to the system default
/// output instead of an explicit rig CODEC.
fn list_audio_devices() {
    let inputs = pancetta_audio::device::list_input_devices();
    let outputs = pancetta_audio::device::list_output_devices();

    println!("Audio input devices:");
    if inputs.is_empty() {
        println!("  (none found)");
    } else {
        for (name, is_default) in &inputs {
            let mark = if *is_default { " (system default)" } else { "" };
            println!("  - {name}{mark}");
        }
    }

    println!();
    println!("Audio output devices:");
    if outputs.is_empty() {
        println!("  (none found)");
    } else {
        for (name, is_default) in &outputs {
            let mark = if *is_default { " (system default)" } else { "" };
            println!("  - {name}{mark}");
        }
    }

    println!();
    println!(
        "Set the rig's CODEC in ~/.pancetta/pancetta.toml under [audio] \
         input_device / output_device."
    );
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
    println!(
        "  ft8_lib C decoder: {}",
        if pancetta_ft8::ft8lib_is_available() {
            "native-C"
        } else {
            "STUB (pure-Rust only — degraded decode recall; fix: git submodule update --init, then rebuild)"
        }
    );
    println!();

    // Audio devices require the audio subsystem — enumerate via `pancetta test-audio --list`.
    println!("Audio devices: (run `pancetta test-audio --list`)");

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
    let (config, _warnings) = load_configuration_with_warnings(cli).await?;
    Ok(config)
}

/// Like [`load_configuration`] but also returns non-fatal config-load warnings
/// (e.g. a `pancetta.toml` that existed but failed to parse and was silently
/// reverted to defaults). The warnings are printed to the console here and also
/// returned so they can be surfaced in the TUI.
async fn load_configuration_with_warnings(cli: &Cli) -> Result<(Config, Vec<String>)> {
    let (mut config, warnings) = if let Some(config_path) = &cli.config {
        let config = Config::load_from_file(config_path)
            .with_context(|| format!("Failed to load config from {}", config_path.display()))?;
        (config, Vec::new())
    } else {
        match Config::load_default_with_warnings() {
            Ok(pair) => pair,
            Err(e)
                if offer_wizard_on_load_failure(
                    cli.headless,
                    cli.wav.is_some(),
                    cli.replay.is_some(),
                    std::io::stdin().is_terminal(),
                ) =>
            {
                eprintln!();
                eprintln!("ERROR: your saved configuration failed to load:");
                eprintln!("  {e}");
                eprintln!("  (file: ~/.pancetta/pancetta.toml)");
                eprintln!();
                if prompt_yes_no(
                    "Re-run first-time setup? (overwrites the broken config on save)",
                    false,
                )? {
                    let defaults = Config::default();
                    match run_first_time_setup(&defaults)? {
                        Some(fixed) => (fixed, Vec::new()),
                        None => {
                            return Err(anyhow::anyhow!(e))
                                .context("Failed to load default configuration")
                        }
                    }
                } else {
                    eprintln!("Edit the file by hand, or delete it to start fresh.");
                    return Err(anyhow::anyhow!(e)).context("Failed to load default configuration");
                }
            }
            Err(e) => {
                return Err(anyhow::anyhow!(e)).context("Failed to load default configuration")
            }
        }
    };

    // Surface parse failures to the console at startup — a broken/partial config
    // silently reverting to defaults (N0CALL, default audio) is exactly the trap
    // we are closing here.
    for w in &warnings {
        eprintln!("WARNING: {w}");
        warn!("{w}");
    }

    // First-run setup: if callsign is still the default, prompt the user.
    // Only run when stdin is a TTY — non-interactive invocations (config --show,
    // piped input) must not trigger the wizard or overwrite the config.
    // `--wav` and `--replay` are both file-fed, unattended runs (scripted demos,
    // VHS recordings, integration tests): a modal wizard would hijack them even
    // on a TTY, so both suppress it.
    let is_interactive = std::io::stdin().is_terminal();
    if config.station.callsign == "N0CALL"
        && !cli.headless
        && cli.wav.is_none()
        && cli.replay.is_none()
        && is_interactive
    {
        if let Some(updated) = run_first_time_setup(&config)? {
            config = updated;
        }
    }

    Ok((config, warnings))
}

/// The de-brick wizard offer fires only for interactive TUI launches —
/// exactly the same gate as the first-run wizard's `is_interactive` check.
/// `wav`/`replay` are file-fed unattended modes; neither may be interrupted
/// by a prompt.
fn offer_wizard_on_load_failure(
    headless: bool,
    wav: bool,
    replay: bool,
    interactive: bool,
) -> bool {
    !headless && !wav && !replay && interactive
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

    // Audio-device setup is the #1 cause of a silent "I see no decodes" first
    // run (wrong/default input, or TX audio routed to laptop speakers). Offer it
    // right here in the first-run flow rather than only via `pancetta setup`.
    if prompt_yes_no(
        "Configure audio input/output devices now? (recommended)",
        true,
    )? {
        setup_audio(&mut new_config)?;
    }

    // Rig / CAT control — the other half of the on-air path. Skippable:
    // decode-only needs no rig, and the rig interface stays disabled by
    // default (safe-by-default posture). Reuses the exact same helpers as
    // `pancetta setup`, including serial-port enumeration.
    if prompt_yes_no(
        "Configure rig CAT control now? (skip = decode-only for now)",
        false,
    )? {
        setup_rig(&mut new_config)?;
        if new_config.rig.interface.enabled {
            setup_ptt(&mut new_config)?;
            setup_frequency(&mut new_config)?;
        }
    }

    if let Err(e) = new_config.validate() {
        println!();
        println!("WARNING: configuration is still invalid ({e}).");
        println!("Not saving — fix the value and re-run, or edit the file by hand.");
        return Ok(None);
    }

    let config_dir = dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".pancetta");
    let config_path = config_dir.join("pancetta.toml");

    if prompt_yes_no(
        &format!("Save configuration to {}?", config_path.display()),
        true,
    )? {
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
    if new_config.rig.interface.enabled {
        println!(
            "Rig:     {} on {} @ {} (PTT: {:?})",
            new_config.rig.model,
            new_config.rig.interface.port,
            new_config.rig.interface.baud_rate,
            new_config.rig.ptt.method
        );
        println!("         Verify the link any time with: pancetta test-rig");
    } else {
        println!("Rig:     not configured — decode-only (no PTT).");
        println!("         To set up CAT later: pancetta setup");
    }
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

/// Maidenhead normalization: field+square uppercase (chars 0-3), subsquare
/// lowercase (chars 4-5), extended digits (6-7) untouched. Matches what
/// `pancetta-config`'s `validate_grid_square` requires.
fn normalize_grid(raw: &str) -> String {
    raw.trim()
        .char_indices()
        .map(|(i, c)| {
            if i < 4 {
                c.to_ascii_uppercase()
            } else {
                c.to_ascii_lowercase()
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
enum StationField {
    Callsign,
    Grid,
}

/// Apply a station-field edit on a clone and run full config validation.
/// Returns the updated Config on success, a user-facing message on failure.
/// Field-level validators in pancetta-config are private by design; whole-
/// config validate() is the public contract.
fn try_set_station_field(
    config: &Config,
    field: StationField,
    value: &str,
) -> std::result::Result<Config, String> {
    let mut candidate = config.clone();
    match field {
        StationField::Callsign => {
            candidate.station.callsign = value.trim().to_uppercase();
        }
        StationField::Grid => {
            candidate.station.grid_square = normalize_grid(value);
        }
    }
    candidate
        .validate()
        .map_err(|e| format!("{e}"))
        .map(|()| candidate)
}

fn setup_station(config: &mut Config) -> Result<()> {
    println!("--- Station ---");
    println!();

    loop {
        let input = prompt_line(&format!("  Callsign [{}]: ", config.station.callsign))?;
        if input.is_empty() {
            break;
        }
        match try_set_station_field(config, StationField::Callsign, &input) {
            Ok(updated) => {
                *config = updated;
                break;
            }
            Err(e) => {
                println!("  Invalid callsign ({e}). Example: K5ARH — try again or Enter to skip.")
            }
        }
    }

    loop {
        let input = prompt_line(&format!("  Grid square [{}]: ", config.station.grid_square))?;
        if input.is_empty() {
            break;
        }
        match try_set_station_field(config, StationField::Grid, &input) {
            Ok(updated) => {
                *config = updated;
                break;
            }
            Err(e) => println!(
                "  Invalid grid ({e}). Example: FN42 or FN42ab — try again or Enter to skip."
            ),
        }
    }

    let input = prompt_line(&format!(
        "  TX power watts [{}]: ",
        config.station.power_watts
    ))?;
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
                    let marker = if info.is_default_input {
                        " (system default)"
                    } else {
                        ""
                    };
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
                    let marker = if info.is_default_output {
                        " (system default)"
                    } else {
                        ""
                    };
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
            let input = prompt_line(&format!("  Input device [{}]: ", config.audio.input_device))?;
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
                &format!(
                    "  Select serial port [current: {}]: ",
                    config.rig.interface.port
                ),
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

    config.rig.frequency.control_enabled = prompt_yes_no(
        "  Enable frequency control?",
        config.rig.frequency.control_enabled,
    )?;

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
    println!(
        "  Station:   {} / {} / {}W",
        config.station.callsign, config.station.grid_square, config.station.power_watts
    );
    println!("  Audio in:  {}", config.audio.input_device);
    println!("  Audio out: {}", config.audio.output_device);
    if config.rig.interface.enabled {
        println!(
            "  Rig:       {} on {} @ {}",
            config.rig.model, config.rig.interface.port, config.rig.interface.baud_rate
        );
        println!("  PTT:       {:?}", config.rig.ptt.method);
        println!(
            "  Freq ctrl: {}",
            if config.rig.frequency.control_enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
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

async fn doctor_command(cli: &Cli) -> Result<()> {
    let ctx = doctor::build_ctx(cli.config.clone());
    let checks = doctor::build_checks();
    println!();
    println!(
        "pancetta doctor — {} checks (config: {})",
        checks.len(),
        ctx.config_path.display()
    );
    println!();
    let mut results = Vec::with_capacity(checks.len());
    for check in &checks {
        let outcome = (check.run)(&ctx);
        println!(
            "[{}] {:<20} {}",
            doctor::status_label(outcome.status),
            check.name,
            outcome.detail
        );
        if outcome.status != doctor::CheckStatus::Pass {
            if let Some(fix) = &outcome.fix {
                println!("       fix: {fix}");
            }
        }
        results.push((check.hard, outcome.status));
    }
    println!();
    if doctor::doctor_exit_code(&results) != 0 {
        println!("Result: NOT READY — fix the FAIL lines above, then re-run `pancetta doctor`.");
        std::process::exit(1);
    }
    println!("Result: ready. Start `pancetta` — you should see decodes within ~30 s.");
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
                println!(
                    "  Check your USB cable and run 'pancetta setup' to select the right port."
                );
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
                    println!(
                        "  Permission denied. You may need to add your user to the 'dialout' group"
                    );
                    println!("  or check device permissions on {}.", port_name);
                }
                serialport::ErrorKind::Io(std::io::ErrorKind::NotFound) => {
                    println!(
                        "  Device not found. The rig may be powered off or USB cable disconnected."
                    );
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
            let hex: Vec<String> = buf[..n.min(16)]
                .iter()
                .map(|b| format!("{:02X}", b))
                .collect();
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
                println!(
                    "       PTT method is 'none' — skipping. Configure PTT in 'pancetta setup'."
                );
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

/// Install a process-wide panic hook so a panic ANYWHERE (including inside a
/// spawned component task — qso/hamlib/autonomous/etc. currently have no
/// `catch_unwind` at all) is guaranteed to reach the file log, not just
/// stderr.
///
/// docs/task-supervision-plan.md problem statement: "no `set_panic_hook`
/// anywhere" — a panic in a background task previously produced only the
/// default Rust panic message on stderr, which is INVISIBLE under the TUI
/// (which owns the terminal via an alternate screen) and easily missed even
/// headless. The task then silently dies: `check_task_handles` polls
/// `is_finished()`, marks it `Failed`, and nothing ever inspected WHY.
///
/// This does not change panic behavior (still `panic = "unwind"`, still
/// unwinds only the panicking task/thread — the process survives) and does
/// not attempt recovery; it only guarantees the panic is *logged with full
/// context* and *counted*, chaining onto the default hook so interactive
/// `cargo run` still shows the familiar stderr message too.
fn install_panic_hook() {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let n = pancetta_lib::coordinator::record_panic();
        let location = info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "<unknown location>".to_string());
        let payload = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<non-string panic payload>".to_string());
        let thread = std::thread::current();
        let thread_name = thread.name().unwrap_or("<unnamed>");
        error!(
            target: "panic",
            "PANIC #{n} on thread '{thread_name}' at {location}: {payload}"
        );
        default_hook(info);
    }));
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

    // Daily rotation with a retention cap so an always-on appliance doesn't
    // accumulate one log file per day forever (security review §5.4). Keep ~2
    // weeks; fall back to the uncapped daily appender if the builder fails.
    let file_appender = tracing_appender::rolling::Builder::new()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix("pancetta.log")
        .max_log_files(14)
        .build(&log_dir)
        .unwrap_or_else(|_| tracing_appender::rolling::daily(&log_dir, "pancetta.log"));
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

    /// docs/task-supervision-plan.md item 4: a panic must be counted and
    /// logged (not just chained to the default hook silently). Uses a
    /// before/after delta rather than an absolute count since the underlying
    /// counter (a private static in `coordinator/health.rs`, read via the
    /// re-exported `pancetta_lib::coordinator::panic_count()`) is
    /// process-global and other tests in this binary may also panic
    /// concurrently.
    #[test]
    fn panic_hook_counts_and_survives_via_catch_unwind() {
        install_panic_hook();
        let before = pancetta_lib::coordinator::panic_count();
        let result = std::panic::catch_unwind(|| {
            panic!("test panic for install_panic_hook coverage");
        });
        assert!(result.is_err(), "catch_unwind should observe the panic");
        let after = pancetta_lib::coordinator::panic_count();
        assert!(
            after > before,
            "panic_count() must increase after a panic (before={before}, after={after})"
        );
    }

    #[test]
    fn test_cli_help() {
        let mut cmd = Command::cargo_bin("pancetta").unwrap();
        cmd.arg("--help");
        cmd.assert()
            .success()
            .stdout(predicate::str::contains("high-performance amateur radio"));
    }

    /// `test-audio --list` enumerates devices and exits 0 (the previous stub
    /// printed "not implemented" and exited 1). We don't assert specific device
    /// names — CI hosts vary — only that the command succeeds and prints the
    /// input/output section headers.
    #[test]
    fn test_cli_test_audio_list_runs() {
        let mut cmd = Command::cargo_bin("pancetta").unwrap();
        cmd.args(["test-audio", "--list"]);
        cmd.assert()
            .success()
            .stdout(predicate::str::contains("Audio input devices:"))
            .stdout(predicate::str::contains("Audio output devices:"));
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

#[cfg(test)]
mod wizard_validation_tests {
    use super::*;
    use pancetta_config::Config;

    #[test]
    fn normalize_grid_uppercases_field_lowercases_subsquare() {
        assert_eq!(normalize_grid("fn42"), "FN42");
        assert_eq!(normalize_grid("fn42AB"), "FN42ab");
        assert_eq!(normalize_grid("FN42ab"), "FN42ab");
        assert_eq!(normalize_grid("fn42ab19"), "FN42ab19");
    }

    #[test]
    fn try_set_station_field_accepts_valid_and_rejects_invalid() {
        let cfg = Config::default();
        // Valid callsign, any case
        let ok = try_set_station_field(&cfg, StationField::Callsign, "k5arh").unwrap();
        assert_eq!(ok.station.callsign, "K5ARH");
        // Letters-only callsign must be rejected (validator requires a digit)
        assert!(try_set_station_field(&cfg, StationField::Callsign, "KARH").is_err());
        // Lowercase grid input must be normalized then accepted
        let ok = try_set_station_field(&cfg, StationField::Grid, "fn42").unwrap();
        assert_eq!(ok.station.grid_square, "FN42");
        // Garbage grid must be rejected, not stored
        assert!(try_set_station_field(&cfg, StationField::Grid, "12ab").is_err());
        assert!(try_set_station_field(&cfg, StationField::Grid, "FN4").is_err());
    }

    #[test]
    fn debrick_gate_only_fires_interactive_tui_runs() {
        assert!(offer_wizard_on_load_failure(false, false, false, true));
        assert!(!offer_wizard_on_load_failure(true, false, false, true)); // headless
        assert!(!offer_wizard_on_load_failure(false, true, false, true)); // --wav
        assert!(!offer_wizard_on_load_failure(false, false, true, true)); // --replay
        assert!(!offer_wizard_on_load_failure(false, false, false, false)); // piped stdin
    }

    // Regression test for the run_first_time_setup validate-then-Ok(Some(..))
    // bug: power_watts is a plain struct field (not routed through
    // try_set_station_field's own validate-and-reject loop like callsign/grid
    // are), so an out-of-range value typed at that prompt used to slip past
    // the wizard's final validate() check and become a live, invalid
    // in-memory config. This confirms the validator this fix relies on
    // actually rejects such a config, so run_first_time_setup's `Ok(None)`
    // branch is reachable and correct.
    #[test]
    fn out_of_range_power_watts_fails_config_validate() {
        let mut cfg = Config::default();
        cfg.station.power_watts = 0;
        assert!(cfg.validate().is_err());

        let mut cfg = Config::default();
        cfg.station.power_watts = 5000;
        assert!(cfg.validate().is_err());
    }
}
