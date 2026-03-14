//! Band hopper example
//! 
//! This example demonstrates automatic band switching based on propagation
//! conditions, time of day, and signal activity. It showcases advanced
//! band management and intelligent frequency selection.

use pancetta_hamlib::prelude::*;
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::{sleep, Instant};
use tracing::{info, warn, debug};
use chrono::{DateTime, Utc, Timelike};

/// Band propagation prediction based on time of day
#[derive(Debug, Clone)]
struct BandConditions {
    /// Band
    band: Band,
    /// Predicted signal strength (relative)
    predicted_strength: f32,
    /// Recommended for this time
    recommended: bool,
    /// Primary use (CW, Phone, Digital)
    primary_use: &'static str,
    /// Best frequency for current conditions
    best_frequency: u64,
}

/// Band hopping strategy
#[derive(Debug, Clone)]
enum HoppingStrategy {
    /// Follow propagation predictions
    Propagation,
    /// Search for activity
    Activity,
    /// Time-based hopping
    TimeOfDay,
    /// Contest frequency hopping
    Contest,
    /// DX hunting
    DxHunting,
}

/// Band hopper configuration
#[derive(Debug, Clone)]
struct BandHopperConfig {
    /// Hopping strategy
    strategy: HoppingStrategy,
    /// Time to spend on each band (seconds)
    dwell_time: u64,
    /// Minimum signal strength to stay on band
    signal_threshold: i32,
    /// Bands to include in hopping
    bands: Vec<Band>,
    /// Preferred modes for each band
    band_modes: HashMap<Band, Mode>,
    /// Enable automatic mode selection
    auto_mode: bool,
    /// Enable activity detection
    activity_detection: bool,
}

impl Default for BandHopperConfig {
    fn default() -> Self {
        let mut band_modes = HashMap::new();
        band_modes.insert(Band::Band40m, Mode::LSB);
        band_modes.insert(Band::Band20m, Mode::USB);
        band_modes.insert(Band::Band15m, Mode::USB);
        band_modes.insert(Band::Band10m, Mode::USB);

        Self {
            strategy: HoppingStrategy::Propagation,
            dwell_time: 30,
            signal_threshold: -80,
            bands: vec![Band::Band40m, Band::Band20m, Band::Band15m, Band::Band10m],
            band_modes,
            auto_mode: true,
            activity_detection: true,
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("info,pancetta_hamlib=debug")
        .init();

    pancetta_hamlib::init();

    info!("Starting band hopper example");

    // Create rig
    let base_rig = RigBuilder::new()
        .build_mock()
        .await?;

    let rig = AdvancedRigBuilder::new()
        .with_mock_rig(base_rig)
        .build()
        .await?;

    // Connect
    rig.connect().await?;
    info!("Connected to rig");

    // Configure band hopper
    let config = BandHopperConfig::default();
    info!("Band hopper configuration: {:?}", config.strategy);

    // Start monitoring for activity detection
    let mut monitor_rx = rig.start_monitoring(1000).await?;
    
    // Run different hopping strategies
    info!("Demonstrating propagation-based hopping...");
    run_propagation_hopping(&rig, &config).await?;

    sleep(Duration::from_secs(2)).await;

    info!("Demonstrating activity-based hopping...");
    run_activity_hopping(&rig, &config, &mut monitor_rx).await?;

    sleep(Duration::from_secs(2)).await;

    info!("Demonstrating time-of-day hopping...");
    run_time_based_hopping(&rig, &config).await?;

    sleep(Duration::from_secs(2)).await;

    info!("Demonstrating contest frequency hopping...");
    run_contest_hopping(&rig, &config).await?;

    sleep(Duration::from_secs(2)).await;

    info!("Demonstrating DX hunting mode...");
    run_dx_hunting(&rig, &config).await?;

    // Stop monitoring
    rig.stop_monitoring().await?;

    // Final summary
    info!("Band hopping demonstration completed");
    
    rig.disconnect().await?;
    Ok(())
}

/// Run propagation-based band hopping
async fn run_propagation_hopping(
    rig: &AdvancedRig,
    config: &BandHopperConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting propagation-based hopping for 60 seconds");

    let start_time = Instant::now();
    let mut current_band_index = 0;
    
    while start_time.elapsed() < Duration::from_secs(60) {
        // Get band conditions
        let conditions = predict_band_conditions();
        
        // Sort bands by predicted conditions
        let mut sorted_bands: Vec<_> = conditions.iter()
            .filter(|c| config.bands.contains(&c.band))
            .collect();
        sorted_bands.sort_by(|a, b| b.predicted_strength.partial_cmp(&a.predicted_strength).unwrap());

        // Select best band
        if let Some(best_condition) = sorted_bands.first() {
            let band = best_condition.band;
            let frequency = best_condition.best_frequency;
            let mode = config.band_modes.get(&band).copied().unwrap_or(Mode::USB);

            info!(
                "Switching to {:?} ({:.3} MHz) - Predicted strength: {:.1}, Recommended: {}",
                band,
                frequency as f64 / 1_000_000.0,
                best_condition.predicted_strength,
                best_condition.recommended
            );

            // Switch to band
            rig.set_frequency(Vfo::A, frequency).await?;
            rig.set_mode(Vfo::A, mode, mode.default_width()).await?;

            // Check actual signal conditions
            sleep(Duration::from_secs(2)).await; // Allow settling time
            
            match rig.get_s_meter().await {
                Ok(s_meter) => {
                    let s_reading = pancetta_hamlib::utils::format_s_meter(s_meter);
                    info!("  Actual signal: {} ({} dBm)", s_reading, s_meter);
                    
                    // Determine dwell time based on signal strength
                    let dwell_time = if s_meter > config.signal_threshold {
                        config.dwell_time * 2 // Stay longer on active band
                    } else {
                        config.dwell_time / 2 // Move on quickly from quiet band
                    };
                    
                    info!("  Dwelling for {} seconds", dwell_time);
                    sleep(Duration::from_secs(dwell_time)).await;
                }
                Err(e) => {
                    warn!("Failed to read signal strength: {}", e);
                    sleep(Duration::from_secs(config.dwell_time)).await;
                }
            }
        }

        current_band_index = (current_band_index + 1) % config.bands.len();
    }

    Ok(())
}

/// Run activity-based band hopping
async fn run_activity_hopping(
    rig: &AdvancedRig,
    config: &BandHopperConfig,
    monitor_rx: &mut tokio::sync::broadcast::Receiver<MonitoringData>,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting activity-based hopping for 45 seconds");

    let start_time = Instant::now();
    let mut band_activity: HashMap<Band, f32> = HashMap::new();

    // Initialize activity scores
    for band in &config.bands {
        band_activity.insert(*band, 0.0);
    }

    while start_time.elapsed() < Duration::from_secs(45) {
        for band in &config.bands {
            // Switch to band
            if let Err(e) = rig.switch_to_band(*band).await {
                warn!("Failed to switch to {:?}: {}", band, e);
                continue;
            }

            let freq = rig.get_frequency(Vfo::A).await?;
            info!("Checking activity on {:?} ({:.3} MHz)", band, freq as f64 / 1_000_000.0);

            // Collect activity data for 3 seconds
            let mut signal_readings = Vec::new();
            let activity_check_start = Instant::now();

            while activity_check_start.elapsed() < Duration::from_secs(3) {
                // Try to get monitoring data
                if let Ok(data) = tokio::time::timeout(Duration::from_millis(100), monitor_rx.recv()).await {
                    if let Ok(monitor_data) = data {
                        if let Some(s_meter) = monitor_data.s_meter {
                            signal_readings.push(s_meter);
                        }
                    }
                }
                
                sleep(Duration::from_millis(100)).await;
            }

            // Calculate activity score
            let activity_score = calculate_activity_score(&signal_readings);
            band_activity.insert(*band, activity_score);

            info!("  Activity score: {:.2} (based on {} readings)", activity_score, signal_readings.len());
        }

        // Find most active band
        let most_active_band = band_activity.iter()
            .max_by(|a, b| a.1.partial_cmp(b.1).unwrap())
            .map(|(band, score)| (*band, *score));

        if let Some((band, score)) = most_active_band {
            info!("Most active band: {:?} (score: {:.2})", band, score);
            
            if score > 0.3 { // Threshold for "active"
                rig.switch_to_band(band).await?;
                let freq = rig.get_frequency(Vfo::A).await?;
                info!("Staying on active band {:?} ({:.3} MHz)", band, freq as f64 / 1_000_000.0);
                
                // Stay longer on active band
                sleep(Duration::from_secs(config.dwell_time * 2)).await;
            } else {
                info!("No significant activity detected, continuing search");
            }
        }
    }

    Ok(())
}

/// Run time-of-day based band hopping
async fn run_time_based_hopping(
    rig: &AdvancedRig,
    config: &BandHopperConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting time-of-day based hopping");

    let now = Utc::now();
    let hour = now.time().hour();
    
    info!("Current UTC time: {} (hour: {})", now.format("%H:%M"), hour);

    // Band recommendations based on time of day (simplified)
    let time_recommendations = get_time_based_recommendations(hour);
    
    info!("Recommended bands for current time:");
    for (band, reason) in &time_recommendations {
        info!("  {:?}: {}", band, reason);
    }

    // Visit recommended bands in order
    for (band, reason) in time_recommendations {
        info!("Visiting {:?} - {}", band, reason);
        
        if let Err(e) = rig.switch_to_band(band).await {
            warn!("Failed to switch to {:?}: {}", band, e);
            continue;
        }

        // Get frequency and mode
        let freq = rig.get_frequency(Vfo::A).await?;
        let (mode, width) = rig.get_mode(Vfo::A).await?;
        
        info!("  Frequency: {:.3} MHz", freq as f64 / 1_000_000.0);
        info!("  Mode: {:?} ({} Hz)", mode, width);

        // Check for signals
        match rig.get_s_meter().await {
            Ok(s_meter) => {
                let s_reading = pancetta_hamlib::utils::format_s_meter(s_meter);
                info!("  Signal: {} ({} dBm)", s_reading, s_meter);
            }
            Err(e) => warn!("  Failed to read signal: {}", e),
        }

        sleep(Duration::from_secs(config.dwell_time)).await;
    }

    Ok(())
}

/// Run contest frequency hopping
async fn run_contest_hopping(
    rig: &AdvancedRig,
    config: &BandHopperConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting contest frequency hopping");

    // Contest frequencies (phone portions of bands)
    let contest_frequencies = vec![
        (Band::Band40m, 7_200_000, Mode::LSB, "40m Phone"),
        (Band::Band20m, 14_200_000, Mode::USB, "20m Phone"),
        (Band::Band20m, 14_250_000, Mode::USB, "20m DX"),
        (Band::Band15m, 21_200_000, Mode::USB, "15m Phone"),
        (Band::Band15m, 21_300_000, Mode::USB, "15m DX"),
        (Band::Band10m, 28_300_000, Mode::USB, "10m Phone"),
        (Band::Band10m, 28_500_000, Mode::USB, "10m DX"),
    ];

    for (band, frequency, mode, description) in contest_frequencies {
        if !config.bands.contains(&band) {
            continue;
        }

        info!("Checking {} ({:.3} MHz {:?})", description, frequency as f64 / 1_000_000.0, mode);

        // Set frequency and mode
        rig.set_frequency(Vfo::A, frequency).await?;
        rig.set_mode(Vfo::A, mode, mode.default_width()).await?;

        // Quick signal check
        sleep(Duration::from_millis(500)).await;

        match rig.get_s_meter().await {
            Ok(s_meter) => {
                let s_reading = pancetta_hamlib::utils::format_s_meter(s_meter);
                info!("  Signal: {} ({} dBm)", s_reading, s_meter);

                // If strong signal, stay longer
                if s_meter > -50 {
                    info!("  Strong signal detected - extended listening");
                    sleep(Duration::from_secs(10)).await;
                } else {
                    sleep(Duration::from_secs(3)).await;
                }
            }
            Err(e) => {
                warn!("  Failed to read signal: {}", e);
                sleep(Duration::from_secs(2)).await;
            }
        }
    }

    Ok(())
}

/// Run DX hunting mode
async fn run_dx_hunting(
    rig: &AdvancedRig,
    config: &BandHopperConfig,
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting DX hunting mode");

    // DX frequencies (band edges and DX portions)
    let dx_frequencies = vec![
        (Band::Band40m, 7_000_000, Mode::CW, "40m CW DX"),
        (Band::Band40m, 7_175_000, Mode::LSB, "40m DX Window"),
        (Band::Band20m, 14_000_000, Mode::CW, "20m CW DX"),
        (Band::Band20m, 14_195_000, Mode::USB, "20m DX Window"),
        (Band::Band15m, 21_000_000, Mode::CW, "15m CW DX"),
        (Band::Band15m, 21_295_000, Mode::USB, "15m DX Window"),
        (Band::Band10m, 28_000_000, Mode::CW, "10m CW DX"),
        (Band::Band10m, 28_495_000, Mode::USB, "10m DX Window"),
    ];

    for (band, frequency, mode, description) in dx_frequencies {
        if !config.bands.contains(&band) {
            continue;
        }

        info!("Hunting DX on {} ({:.3} MHz {:?})", description, frequency as f64 / 1_000_000.0, mode);

        // Set frequency and mode
        rig.set_frequency(Vfo::A, frequency).await?;
        rig.set_mode(Vfo::A, mode, mode.default_width()).await?;

        // Extended listening for DX
        sleep(Duration::from_secs(1)).await;

        match rig.get_s_meter().await {
            Ok(s_meter) => {
                let s_reading = pancetta_hamlib::utils::format_s_meter(s_meter);
                info!("  Signal: {} ({} dBm)", s_reading, s_meter);

                // DX hunting logic - look for specific signal patterns
                if s_meter > -70 && s_meter < -40 {
                    info!("  Potential DX signal strength - extended monitoring");
                    
                    // Monitor signal for variation (possible DX fade)
                    let mut readings = Vec::new();
                    for _ in 0..10 {
                        sleep(Duration::from_millis(500)).await;
                        if let Ok(reading) = rig.get_s_meter().await {
                            readings.push(reading);
                        }
                    }

                    if !readings.is_empty() {
                        let variation = calculate_signal_variation(&readings);
                        info!("  Signal variation: {:.1} dB", variation);
                        
                        if variation > 6.0 {
                            info!("  High signal variation - possible DX QSB!");
                            sleep(Duration::from_secs(15)).await; // Extended listening
                        }
                    }
                } else {
                    sleep(Duration::from_secs(3)).await;
                }
            }
            Err(e) => {
                warn!("  Failed to read signal: {}", e);
                sleep(Duration::from_secs(2)).await;
            }
        }
    }

    Ok(())
}

/// Predict band conditions based on current time
fn predict_band_conditions() -> Vec<BandConditions> {
    let now = Utc::now();
    let hour = now.time().hour();
    
    // Simplified propagation prediction
    vec![
        BandConditions {
            band: Band::Band40m,
            predicted_strength: if hour >= 20 || hour <= 6 { 0.9 } else { 0.3 },
            recommended: hour >= 20 || hour <= 6,
            primary_use: "Phone/CW",
            best_frequency: 7_200_000,
        },
        BandConditions {
            band: Band::Band20m,
            predicted_strength: if hour >= 8 && hour <= 18 { 0.8 } else { 0.4 },
            recommended: hour >= 8 && hour <= 18,
            primary_use: "DX/Phone",
            best_frequency: 14_200_000,
        },
        BandConditions {
            band: Band::Band15m,
            predicted_strength: if hour >= 10 && hour <= 16 { 0.7 } else { 0.2 },
            recommended: hour >= 10 && hour <= 16,
            primary_use: "DX",
            best_frequency: 21_200_000,
        },
        BandConditions {
            band: Band::Band10m,
            predicted_strength: if hour >= 12 && hour <= 14 { 0.6 } else { 0.1 },
            recommended: hour >= 12 && hour <= 14,
            primary_use: "DX/Contest",
            best_frequency: 28_300_000,
        },
    ]
}

/// Get band recommendations based on time of day
fn get_time_based_recommendations(hour: u32) -> Vec<(Band, String)> {
    match hour {
        0..=6 => vec![
            (Band::Band40m, "Night time - excellent local/regional".to_string()),
            (Band::Band80m, "Night time - good for local".to_string()),
        ],
        7..=9 => vec![
            (Band::Band40m, "Morning - good for short skip".to_string()),
            (Band::Band20m, "Morning - opening for DX".to_string()),
        ],
        10..=16 => vec![
            (Band::Band20m, "Daytime - excellent for DX".to_string()),
            (Band::Band15m, "Daytime - good for DX when open".to_string()),
            (Band::Band10m, "Daytime - check for openings".to_string()),
        ],
        17..=19 => vec![
            (Band::Band20m, "Evening - still good for DX".to_string()),
            (Band::Band40m, "Evening - transition period".to_string()),
        ],
        20..=23 => vec![
            (Band::Band40m, "Night time - excellent".to_string()),
            (Band::Band80m, "Night time - very good".to_string()),
        ],
        _ => vec![(Band::Band20m, "Default band".to_string())],
    }
}

/// Calculate activity score from signal readings
fn calculate_activity_score(readings: &[i32]) -> f32 {
    if readings.is_empty() {
        return 0.0;
    }

    let avg = readings.iter().sum::<i32>() as f32 / readings.len() as f32;
    let variation = calculate_signal_variation(readings);
    
    // Higher average signal and more variation indicate activity
    let signal_score = ((avg + 120.0) / 120.0).max(0.0).min(1.0); // Normalize -120 to 0 dBm
    let variation_score = (variation / 20.0).max(0.0).min(1.0); // Normalize variation
    
    (signal_score + variation_score) / 2.0
}

/// Calculate signal variation (standard deviation)
fn calculate_signal_variation(readings: &[i32]) -> f32 {
    if readings.len() < 2 {
        return 0.0;
    }

    let avg = readings.iter().sum::<i32>() as f32 / readings.len() as f32;
    let variance = readings.iter()
        .map(|&x| (x as f32 - avg).powi(2))
        .sum::<f32>() / readings.len() as f32;
    
    variance.sqrt()
}