//! Frequency scanner example
//!
//! This example demonstrates advanced scanning capabilities including
//! memory channel scanning, frequency range scanning, and real-time
//! signal monitoring.

use pancetta_hamlib::prelude::*;
use std::time::Duration;
use tokio::time::{sleep, timeout};
use tracing::{error, info, warn};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging with detailed output
    tracing_subscriber::fmt()
        .with_env_filter("info,pancetta_hamlib=debug")
        .init();

    // Initialize hamlib
    pancetta_hamlib::init();

    info!("Starting frequency scanner example");

    // Create mock rig for demonstration
    let base_rig = RigBuilder::new().build_mock().await?;

    // Create advanced rig controller
    let rig = AdvancedRigBuilder::new()
        .with_mock_rig(base_rig)
        .build()
        .await?;

    // Connect to rig
    rig.connect().await?;
    info!("Connected to rig");

    // Demonstrate memory channel programming
    info!("Programming memory channels...");
    program_memory_channels(&rig).await?;

    // List programmed memory channels
    let memory_channels = rig.list_memory_channels().await?;
    info!("Programmed {} memory channels:", memory_channels.len());
    for channel in &memory_channels {
        info!(
            "  Ch {}: {:.3} MHz {:?} {}",
            channel.channel,
            channel.frequency as f64 / 1_000_000.0,
            channel.mode,
            channel.name.as_deref().unwrap_or("Unnamed")
        );
    }

    // Demonstrate memory channel scanning
    info!("Starting memory channel scan...");

    let scan_config = ScanConfig {
        scan_type: ScanType::Memory,
        speed: 3.0,             // 3 channels per second
        pause_time: 2.0,        // 2 seconds on active signal
        resume_timeout: 5.0,    // 5 seconds before resuming
        squelch_threshold: -80, // -80 dBm threshold
        include_memory: true,
        include_vfo: false,
        custom_frequencies: vec![],
    };

    // Start scanning in background
    rig.start_scan(scan_config).await?;

    // Monitor scan progress for 30 seconds
    let scan_duration = Duration::from_secs(30);
    let start_time = std::time::Instant::now();

    info!("Monitoring scan for {} seconds...", scan_duration.as_secs());

    while start_time.elapsed() < scan_duration {
        sleep(Duration::from_secs(2)).await;

        match rig.get_scan_status().await {
            Ok(status) => {
                if status.active {
                    info!(
                        "Scan active - Position: {}, Channels: {}, Signals: {}",
                        status.current_position, status.channels_scanned, status.signals_found
                    );
                } else {
                    info!("Scan stopped");
                    break;
                }
            }
            Err(e) => warn!("Failed to get scan status: {}", e),
        }
    }

    // Stop scanning
    rig.stop_scan().await?;
    info!("Memory channel scan completed");

    // Demonstrate frequency range scanning
    info!("Starting 20m band frequency scan...");

    let band_20m = Band::Band20m;
    let (start_freq, end_freq) = band_20m.frequency_range();

    // Create custom frequency list for 20m band (25 kHz steps)
    let mut custom_frequencies = Vec::new();
    let mut freq = start_freq;
    while freq <= end_freq {
        custom_frequencies.push(freq);
        freq += 25_000; // 25 kHz steps
    }

    info!(
        "Scanning {} frequencies from {:.3} to {:.3} MHz",
        custom_frequencies.len(),
        start_freq as f64 / 1_000_000.0,
        end_freq as f64 / 1_000_000.0
    );

    let range_scan_config = ScanConfig {
        scan_type: ScanType::Custom,
        speed: 10.0, // 10 frequencies per second
        pause_time: 1.0,
        resume_timeout: 3.0,
        squelch_threshold: -70, // More sensitive for band scanning
        include_memory: false,
        include_vfo: false,
        custom_frequencies,
    };

    // Start range scanning
    rig.start_scan(range_scan_config).await?;

    // Monitor range scan for 20 seconds
    let range_scan_duration = Duration::from_secs(20);
    let range_start_time = std::time::Instant::now();

    while range_start_time.elapsed() < range_scan_duration {
        sleep(Duration::from_secs(1)).await;

        match rig.get_scan_status().await {
            Ok(status) => {
                if status.active {
                    info!(
                        "Range scan - Position: {}, Scanned: {}, Signals: {}",
                        status.current_position, status.channels_scanned, status.signals_found
                    );
                } else {
                    break;
                }
            }
            Err(e) => warn!("Failed to get scan status: {}", e),
        }
    }

    // Stop range scanning
    rig.stop_scan().await?;
    info!("Frequency range scan completed");

    // Demonstrate real-time monitoring
    info!("Starting real-time monitoring...");

    // Set to a specific frequency for monitoring
    rig.set_frequency(Vfo::A, 14_200_000).await?;
    rig.set_mode(Vfo::A, Mode::USB, Some(2400)).await?;

    // Start monitoring with 500ms updates
    let mut monitor_rx = rig.start_monitoring(500).await?;

    info!("Monitoring rig parameters for 15 seconds...");

    // Monitor for 15 seconds or until no more data
    let monitor_result = timeout(Duration::from_secs(15), async {
        while let Ok(data) = monitor_rx.recv().await {
            let freq_str = data
                .frequency
                .map(|f| format!("{:.3} MHz", f as f64 / 1_000_000.0))
                .unwrap_or_else(|| "Unknown".to_string());

            let mode_str = data
                .mode
                .map(|m| format!("{:?}", m))
                .unwrap_or_else(|| "Unknown".to_string());

            let s_meter_str = data
                .s_meter
                .map(|s| pancetta_hamlib::utils::format_s_meter(s))
                .unwrap_or_else(|| "N/A".to_string());

            let swr_str = data
                .swr
                .map(|s| pancetta_hamlib::utils::format_swr(s))
                .unwrap_or_else(|| "N/A".to_string());

            let ptt_str = if data.ptt_active { "TX" } else { "RX" };

            info!(
                "Monitor - Freq: {}, Mode: {}, S-meter: {}, SWR: {}, PTT: {}",
                freq_str, mode_str, s_meter_str, swr_str, ptt_str
            );

            // Brief pause to avoid flooding logs
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await;

    match monitor_result {
        Ok(_) => info!("Monitoring completed normally"),
        Err(_) => info!("Monitoring timed out"),
    }

    // Stop monitoring
    rig.stop_monitoring().await?;

    // Demonstrate band switching
    info!("Demonstrating band switching...");

    let bands_to_test = vec![Band::Band40m, Band::Band20m, Band::Band15m, Band::Band10m];

    for band in bands_to_test {
        info!("Switching to {:?} band", band);

        if let Err(e) = rig.switch_to_band(band).await {
            warn!("Failed to switch to {:?}: {}", band, e);
            continue;
        }

        // Get current settings after band switch
        let freq = rig.get_frequency(Vfo::A).await?;
        let (mode, width) = rig.get_mode(Vfo::A).await?;

        info!(
            "  Band {:?}: {:.3} MHz, {:?}, {} Hz",
            band,
            freq as f64 / 1_000_000.0,
            mode,
            width
        );

        // Read signal level on this band
        match rig.get_s_meter().await {
            Ok(s_meter) => {
                let s_reading = pancetta_hamlib::utils::format_s_meter(s_meter);
                info!("  Signal: {} ({} dBm)", s_reading, s_meter);
            }
            Err(e) => warn!("  Failed to read signal: {}", e),
        }

        sleep(Duration::from_millis(500)).await;
    }

    // Demonstrate priority channel monitoring
    info!("Setting up priority channel monitoring...");

    // Program a priority channel
    rig.set_frequency(Vfo::A, 14_300_000).await?; // Emergency frequency
    rig.set_mode(Vfo::A, Mode::USB, Some(2400)).await?;
    rig.save_to_memory(99, Some("Priority/Emergency".to_string()))
        .await?;

    info!("Priority channel programmed: 14.300 MHz USB");

    // Final status
    let final_status = rig.get_status().await?;
    info!("Scanner example completed");
    info!("Final status: {:?}", final_status.connection_state);

    // Disconnect
    rig.disconnect().await?;
    info!("Disconnected from rig");

    Ok(())
}

/// Program sample memory channels for scanning demonstration
async fn program_memory_channels(rig: &AdvancedRig) -> Result<(), Box<dyn std::error::Error>> {
    let channels = vec![
        (1, 14_200_000, Mode::USB, "20m Phone"),
        (2, 14_074_000, Mode::FT8, "20m FT8"),
        (3, 14_080_000, Mode::FT4, "20m FT4"),
        (4, 7_200_000, Mode::LSB, "40m Phone"),
        (5, 7_074_000, Mode::FT8, "40m FT8"),
        (6, 21_200_000, Mode::USB, "15m Phone"),
        (7, 21_074_000, Mode::FT8, "15m FT8"),
        (8, 28_200_000, Mode::USB, "10m Phone"),
        (9, 28_074_000, Mode::FT8, "10m FT8"),
        (10, 29_600_000, Mode::FM, "10m FM"),
    ];

    for (channel, frequency, mode, name) in channels {
        // Set frequency and mode
        rig.set_frequency(Vfo::A, frequency).await?;
        rig.set_mode(Vfo::A, mode, mode.default_width()).await?;

        // Save to memory
        rig.save_to_memory(channel, Some(name.to_string())).await?;

        info!(
            "Programmed channel {}: {:.3} MHz {:?} ({})",
            channel,
            frequency as f64 / 1_000_000.0,
            mode,
            name
        );
    }

    Ok(())
}

/// Analyze scan results and provide summary
fn analyze_scan_results(status: &ScanStatus) {
    if let Some(start_time) = status.start_time {
        let duration = start_time.elapsed();
        let scan_rate = if duration.as_secs() > 0 {
            status.channels_scanned as f64 / duration.as_secs() as f64
        } else {
            0.0
        };

        info!("Scan Analysis:");
        info!("  Duration: {:.1} seconds", duration.as_secs_f64());
        info!("  Channels scanned: {}", status.channels_scanned);
        info!("  Signals found: {}", status.signals_found);
        info!("  Scan rate: {:.1} channels/second", scan_rate);

        if status.channels_scanned > 0 {
            let hit_rate = (status.signals_found as f64 / status.channels_scanned as f64) * 100.0;
            info!("  Hit rate: {:.1}%", hit_rate);
        }
    }
}
