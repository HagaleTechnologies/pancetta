//! Contest logging example
//!
//! This example demonstrates contest-specific QSO logging including:
//! - Contest mode configuration
//! - Serial number tracking
//! - Automatic logging
//! - ADIF export with contest data
//! - Contest statistics

use pancetta_qso::*;
use std::error::Error;
use std::path::PathBuf;
use tempfile::tempdir;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    println!("=== Pancetta QSO Contest Logging Example ===");

    // Create temporary directory for this example
    let temp_dir = tempdir()?;
    let db_path = temp_dir.path().join("contest_qsos.db");
    let export_dir = temp_dir.path().join("exports");
    std::fs::create_dir_all(&export_dir)?;

    // Configure contest mode
    let contest_config = ContestConfig {
        contest_name: "ARRL DX Contest".to_string(),
        category: "Single Op All Band".to_string(),
        starting_serial: 1,
        enabled: true,
    };

    // Configure QSO manager with contest mode
    let qso_config = QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        contest_mode: Some(contest_config),
        ..Default::default()
    };

    // Configure logger with automatic features
    let logger_config = LoggerConfig {
        database_path: db_path.clone(),
        auto_logging: AutoLoggingConfig {
            enabled: true,
            min_duration: 10, // 10 seconds minimum for contest
            require_both_reports: true,
            auto_export: true,
            ..Default::default()
        },
        export_import: ExportImportConfig {
            export_directory: export_dir.clone(),
            export_formats: vec![ExportFormat::Adif, ExportFormat::Csv],
            ..Default::default()
        },
        ..Default::default()
    };

    // Create contest logging system
    let system = QsoSystemBuilder::new()
        .with_qso_config(qso_config)
        .with_logger(logger_config)
        .build()
        .await?;

    println!("✓ Contest logging system initialized");
    println!("  Database: {:?}", db_path);
    println!("  Export directory: {:?}", export_dir);

    // Simulate several contest QSOs
    let contest_qsos = vec![
        ("K1DEF", "FN31", 14074000.0, -12, -15),
        ("VE3XYZ", "FN03", 14074000.0, -18, -21),
        ("G0ABC", "IO91", 21074000.0, -15, -12),
        ("JA1QSO", "PM95", 21074000.0, -20, -18),
        ("PY2DEF", "GG66", 28074000.0, -8, -10),
    ];

    println!("\n=== Simulating Contest QSOs ===");

    for (i, (their_call, their_grid, frequency, our_report, their_report)) in
        contest_qsos.iter().enumerate()
    {
        let serial = i as u32 + 1;

        println!("\n📡 Contest QSO #{}: {}", serial, their_call);
        println!("  Frequency: {:.3} MHz", frequency / 1_000_000.0);
        println!("  Grid: {}", their_grid);
        println!("  Reports: {:+}/{:+} dB", our_report, their_report);

        // Start QSO by responding to their CQ
        let qso_id = system
            .respond_to_cq(their_call.to_string(), *frequency)
            .await?;

        // Simulate contest exchange with serial numbers
        let contest_exchange = MessageType::ContestExchange {
            to_station: their_call.to_string(),
            from_station: "W1ABC".to_string(),
            report: *our_report,
            serial: serial,
        };

        // Process their contest exchange
        let their_exchange = MessageType::ContestExchange {
            to_station: "W1ABC".to_string(),
            from_station: their_call.to_string(),
            report: *their_report,
            serial: serial * 10, // Simulate different serial from them
        };

        system
            .process_message(
                their_exchange,
                format!(
                    "W1ABC {} {:03} {:03}",
                    their_call,
                    their_report + 35,
                    serial * 10
                ),
                *frequency,
                Some(*their_report as f32),
            )
            .await?;

        // Simulate QSO completion
        tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;

        let confirmation = MessageType::FinalConfirmation {
            to_station: their_call.to_string(),
            from_station: "W1ABC".to_string(),
        };

        // Complete the QSO
        let seventy_three = MessageType::SeventyThree {
            to_station: "W1ABC".to_string(),
            from_station: their_call.to_string(),
        };

        system
            .process_message(
                seventy_three,
                format!("W1ABC {} 73", their_call),
                *frequency,
                Some(*their_report as f32),
            )
            .await?;

        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // Check QSO completion
        let progress = system.get_qso(qso_id).await?;
        if progress.state.is_terminal() {
            println!("  ✓ QSO completed and logged");

            // Manually log the QSO with contest information if logger is available
            if let Some(ref logger) = system.logger {
                // Create contest metadata
                let mut contest_metadata = progress.metadata.clone();
                contest_metadata.contest_info = Some(ContestInfo {
                    contest_name: "ARRL DX Contest".to_string(),
                    category: "Single Op All Band".to_string(),
                    serials: ContestSerials {
                        sent: Some(serial),
                        received: Some(serial * 10),
                    },
                    points: 3, // DX contact = 3 points
                    multiplier: Some(their_grid.to_string()),
                });

                let contest_progress = QsoProgress {
                    state: progress.state.clone(),
                    state_history: progress.state_history,
                    messages: progress.messages,
                    metadata: contest_metadata,
                };

                logger.log_qso(&contest_progress).await?;
                println!("  ✓ Contest data logged");
            }
        }
    }

    // Get contest statistics if logger is available
    if let Some(ref logger) = system.logger {
        println!("\n=== Contest Statistics ===");

        let stats = logger.get_statistics().await?;
        println!("Total QSOs logged: {}", stats.total_qsos);
        println!("Confirmed QSOs: {}", stats.confirmed_qsos);
        println!("Unique callsigns: {}", stats.unique_callsigns);

        println!("\nQSOs by band:");
        for (band, count) in &stats.qsos_by_band {
            println!("  {}: {}", band, count);
        }

        println!("\nQSOs by mode:");
        for (mode, count) in &stats.qsos_by_mode {
            println!("  {}: {}", mode, count);
        }

        // Export contest log to ADIF
        println!("\n=== Exporting Contest Log ===");

        let adif_path = export_dir.join("contest_log.adi");
        let export_result = logger.export_adif(&adif_path, None).await?;

        println!("✓ Exported {} QSOs to ADIF", export_result.qso_count);
        println!("  File: {:?}", adif_path);
        println!("  Size: {} bytes", export_result.file_size);

        // Export to CSV as well
        let csv_path = export_dir.join("contest_log.csv");
        let csv_result = logger.export_csv(&csv_path, None).await?;

        println!("✓ Exported {} QSOs to CSV", csv_result.qso_count);
        println!("  File: {:?}", csv_path);

        // Show export history
        println!("\n=== Export History ===");
        let export_history = logger.get_export_history().await;
        for (i, export) in export_history.iter().enumerate() {
            println!(
                "{}. {:?} - {} QSOs - {} bytes - {}",
                i + 1,
                export.format,
                export.qso_count,
                export.file_size,
                export.timestamp.format("%Y-%m-%d %H:%M:%S UTC")
            );
        }

        // Demonstrate contest-specific queries
        println!("\n=== Contest Queries ===");

        // Search for QSOs in a specific band
        let band_filter = QsoFilter {
            band: Some("20M".to_string()),
            ..Default::default()
        };

        let band_qsos = logger
            .search_qsos(&band_filter, &QueryOptions::default())
            .await?;
        println!("QSOs on 20M: {}", band_qsos.len());

        // Search for QSOs above a certain signal strength
        let strong_filter = QsoFilter {
            min_signal_strength: Some(-15),
            ..Default::default()
        };

        let strong_qsos = logger
            .search_qsos(&strong_filter, &QueryOptions::default())
            .await?;
        println!("QSOs with signal ≥ -15 dB: {}", strong_qsos.len());

        // Search for QSOs with specific grid pattern (FN grid)
        let grid_filter = QsoFilter {
            grid_pattern: Some("FN".to_string()),
            ..Default::default()
        };

        let grid_qsos = logger
            .search_qsos(&grid_filter, &QueryOptions::default())
            .await?;
        println!("QSOs in FN grid squares: {}", grid_qsos.len());

        // Read and display part of the exported ADIF file
        if adif_path.exists() {
            println!("\n=== ADIF Export Sample ===");
            let adif_content = std::fs::read_to_string(&adif_path)?;
            let lines: Vec<&str> = adif_content.lines().take(15).collect();
            for line in lines {
                if !line.trim().is_empty() {
                    println!("{}", line);
                }
            }
            if adif_content.lines().count() > 15 {
                println!("... (truncated)");
            }
        }

        // Calculate contest score estimate
        println!("\n=== Contest Score Estimate ===");
        let total_points = contest_qsos.len() * 3; // 3 points per DX contact
        let multipliers = contest_qsos.len(); // Each unique grid square
        let estimated_score = total_points * multipliers;

        println!("QSOs: {}", contest_qsos.len());
        println!("Points: {} (3 per DX QSO)", total_points);
        println!("Multipliers: {} (unique grids)", multipliers);
        println!("Estimated Score: {} points", estimated_score);
    }

    println!("\n✓ Contest logging example completed!");
    println!("  Database preserved at: {:?}", db_path);
    println!("  Exports available at: {:?}", export_dir);

    Ok(())
}
