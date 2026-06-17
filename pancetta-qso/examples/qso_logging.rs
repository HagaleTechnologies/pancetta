//! QSO logging example
//!
//! This example demonstrates async QSO logging with `QsoLogger` including:
//! - System configuration and initialisation via `QsoSystemBuilder`
//! - Logging multiple QSOs through the async logger
//! - ADIF export using `QsoLogger::export_adif`
//! - Querying basic statistics

use pancetta_qso::*;
use std::error::Error;
use tempfile::tempdir;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // Initialize logging
    tracing_subscriber::fmt::init();

    println!("=== Pancetta QSO Logging Example ===");

    // Create temporary directory for this example
    let temp_dir = tempdir()?;
    let db_path = temp_dir.path().join("qsos.db");
    let export_dir = temp_dir.path().join("exports");
    std::fs::create_dir_all(&export_dir)?;

    // Configure QSO manager
    let qso_config = QsoManagerConfig {
        our_callsign: "W1ABC".to_string(),
        our_grid: Some("FN42".to_string()),
        ..Default::default()
    };

    // Configure logger with automatic features and ADIF export
    let logger_config = LoggerConfig {
        database_path: db_path.clone(),
        auto_logging: AutoLoggingConfig {
            enabled: true,
            min_duration: 10, // 10 seconds minimum QSO duration
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

    // Create the logging system
    let system = QsoSystemBuilder::new()
        .with_qso_config(qso_config)
        .with_logger(logger_config)
        .build()
        .await?;

    println!("  Database: {:?}", db_path);
    println!("  Export directory: {:?}", export_dir);

    // Sample QSOs to log (callsign, grid, frequency Hz, our SNR, their SNR)
    let sample_qsos = vec![
        ("K1DEF", "FN31", 14074000.0f64, -12i32, -15i32),
        ("VE3XYZ", "FN03", 14074000.0, -18, -21),
        ("G0ABC", "IO91", 21074000.0, -15, -12),
        ("JA1QSO", "PM95", 21074000.0, -20, -18),
        ("PY2DEF", "GG66", 28074000.0, -8, -10),
    ];

    println!("\n=== Simulating QSOs ===");

    for (their_call, their_grid, frequency, our_report, their_report) in sample_qsos.iter() {
        println!(
            "\n  {}: {:.3} MHz, grid {}, reports {:+}/{:+} dB",
            their_call,
            frequency / 1_000_000.0,
            their_grid,
            our_report,
            their_report,
        );

        // Start QSO by responding to their CQ
        let qso_id = system
            .respond_to_cq(their_call.to_string(), *frequency)
            .await?;

        // Complete the QSO — process their 73
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

        // Check QSO completion and log it
        let progress = system.get_qso(qso_id).await?;
        if progress.state.is_terminal() {
            if let Some(ref logger) = system.logger {
                logger.log_qso(&progress).await?;
            }
            println!("  QSO completed and logged");
        }
    }

    // Statistics and ADIF export
    if let Some(ref logger) = system.logger {
        println!("\n=== Statistics ===");

        let stats = logger.get_statistics().await?;
        println!("Total QSOs logged: {}", stats.total_qsos);
        println!("Unique callsigns:  {}", stats.unique_callsigns);
        println!("Bands worked:      {}", stats.bands_worked);
        println!("Modes worked:      {}", stats.modes_worked);

        println!("\n=== ADIF Export ===");

        let adif_path = export_dir.join("qso_log.adi");
        let export_result = logger.export_adif(&adif_path, None).await?;

        println!("Exported {} QSOs to ADIF", export_result.qso_count);
        println!("  File: {:?}", adif_path);
        println!("  Size: {} bytes", export_result.file_size);

        if adif_path.exists() {
            println!("\n=== ADIF Sample (first 15 lines) ===");
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
    }

    println!("\nQSO logging example completed.");
    Ok(())
}
