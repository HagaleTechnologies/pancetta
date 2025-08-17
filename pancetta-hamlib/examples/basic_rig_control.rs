//! Basic rig control example
//! 
//! This example demonstrates the fundamental operations for controlling
//! an amateur radio transceiver using the pancetta-hamlib crate.

use pancetta_hamlib::prelude::*;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{info, warn, error};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .init();

    // Initialize the hamlib library
    pancetta_hamlib::init();

    info!("Starting basic rig control example");

    // Create a mock rig for demonstration
    // In real usage, you would specify an actual rig model and device path
    let rig = RigBuilder::new()
        .build_mock()
        .await?;

    info!("Created mock rig for demonstration");

    // Connect to the rig
    match rig.connect().await {
        Ok(()) => info!("Successfully connected to rig"),
        Err(e) => {
            error!("Failed to connect to rig: {}", e);
            return Err(e.into());
        }
    }

    // Get initial rig status
    let status = rig.get_status().await?;
    info!("Initial rig status: {:?}", status.connection_state);

    // Set frequency to 14.200 MHz (20 meters)
    let frequency = 14_200_000; // Hz
    info!("Setting frequency to {:.3} MHz", frequency as f64 / 1_000_000.0);
    
    if let Err(e) = rig.set_frequency(Vfo::A, frequency).await {
        warn!("Failed to set frequency: {}", e);
    } else {
        info!("Frequency set successfully");
    }

    // Verify frequency was set
    match rig.get_frequency(Vfo::A).await {
        Ok(freq) => info!("Current frequency: {:.3} MHz", freq as f64 / 1_000_000.0),
        Err(e) => warn!("Failed to read frequency: {}", e),
    }

    // Set mode to USB with 2.4 kHz bandwidth
    info!("Setting mode to USB with 2.4 kHz bandwidth");
    
    if let Err(e) = rig.set_mode(Vfo::A, Mode::USB, Some(2400)).await {
        warn!("Failed to set mode: {}", e);
    } else {
        info!("Mode set successfully");
    }

    // Verify mode was set
    match rig.get_mode(Vfo::A).await {
        Ok((mode, width)) => info!("Current mode: {:?} with {} Hz bandwidth", mode, width),
        Err(e) => warn!("Failed to read mode: {}", e),
    }

    // Demonstrate VFO switching
    info!("Switching to VFO B");
    if let Err(e) = rig.set_vfo(Vfo::B).await {
        warn!("Failed to switch VFO: {}", e);
    } else {
        info!("Switched to VFO B");
    }

    // Set different frequency on VFO B
    let freq_b = 21_200_000; // 15 meters
    info!("Setting VFO B to {:.3} MHz", freq_b as f64 / 1_000_000.0);
    
    if let Err(e) = rig.set_frequency(Vfo::B, freq_b).await {
        warn!("Failed to set VFO B frequency: {}", e);
    }

    // Switch back to VFO A
    info!("Switching back to VFO A");
    if let Err(e) = rig.set_vfo(Vfo::A).await {
        warn!("Failed to switch back to VFO A: {}", e);
    }

    // Demonstrate power level control
    info!("Setting power level to 50%");
    if let Err(e) = rig.set_power_level(0.5).await {
        warn!("Failed to set power level: {}", e);
    } else {
        match rig.get_power_level().await {
            Ok(power) => info!("Power level set to {:.1}%", power * 100.0),
            Err(e) => warn!("Failed to read power level: {}", e),
        }
    }

    // Demonstrate monitoring capabilities
    info!("Reading rig telemetry...");
    
    // Read S-meter
    match rig.get_s_meter().await {
        Ok(s_meter) => {
            let s_reading = pancetta_hamlib::utils::format_s_meter(s_meter);
            info!("S-meter reading: {} ({} dBm)", s_reading, s_meter);
        }
        Err(e) => warn!("Failed to read S-meter: {}", e),
    }

    // Read SWR
    match rig.get_swr().await {
        Ok(swr) => {
            let swr_reading = pancetta_hamlib::utils::format_swr(swr);
            info!("SWR reading: {}", swr_reading);
        }
        Err(e) => warn!("Failed to read SWR: {}", e),
    }

    // Demonstrate PTT control (be careful with real rigs!)
    info!("Testing PTT control (transmit for 1 second)");
    
    // Turn PTT on
    if let Err(e) = rig.set_ptt(Vfo::A, PttState::On).await {
        warn!("Failed to set PTT on: {}", e);
    } else {
        info!("PTT ON - Transmitting");
        
        // Brief transmission
        sleep(Duration::from_secs(1)).await;
        
        // Turn PTT off
        if let Err(e) = rig.set_ptt(Vfo::A, PttState::Off).await {
            error!("Failed to set PTT off: {}", e);
        } else {
            info!("PTT OFF - Receiving");
        }
    }

    // Get rig information
    match rig.get_info().await {
        Ok(info) => info!("Rig info: {}", info),
        Err(e) => warn!("Failed to get rig info: {}", e),
    }

    // Demonstrate error handling
    info!("Testing error handling with invalid frequency");
    
    // Try to set an invalid frequency (outside amateur bands)
    let invalid_freq = 1_000_000_000; // 1 GHz
    match rig.set_frequency(Vfo::A, invalid_freq).await {
        Ok(()) => warn!("Unexpectedly succeeded with invalid frequency"),
        Err(e) => info!("Correctly rejected invalid frequency: {}", e),
    }

    // Final status check
    let final_status = rig.get_status().await?;
    info!("Final rig status:");
    info!("  Connection: {:?}", final_status.connection_state);
    if let Some(freq) = final_status.frequency {
        info!("  Frequency: {:.3} MHz", freq as f64 / 1_000_000.0);
    }
    if let Some(mode) = final_status.mode {
        info!("  Mode: {:?}", mode);
    }
    if let Some(ptt) = final_status.ptt {
        info!("  PTT: {:?}", ptt);
    }

    // Disconnect from rig
    info!("Disconnecting from rig");
    if let Err(e) = rig.disconnect().await {
        warn!("Error during disconnect: {}", e);
    } else {
        info!("Successfully disconnected");
    }

    info!("Basic rig control example completed");
    Ok(())
}

/// Helper function to demonstrate frequency validation
async fn validate_and_set_frequency(
    rig: &MockRig,
    vfo: Vfo,
    frequency: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    // Check if frequency is in amateur bands
    if !pancetta_hamlib::utils::is_amateur_frequency(frequency) {
        warn!(
            "Frequency {:.3} MHz is not in amateur radio bands",
            frequency as f64 / 1_000_000.0
        );
        return Ok(()); // Don't attempt to set
    }

    // Determine appropriate mode for frequency
    if let Some(band) = pancetta_hamlib::utils::frequency_to_band(frequency) {
        let mode = pancetta_hamlib::utils::band_default_mode(band);
        info!(
            "Setting {:.3} MHz ({:?}) with mode {:?}",
            frequency as f64 / 1_000_000.0,
            band,
            mode
        );
        
        rig.set_frequency(vfo, frequency).await?;
        rig.set_mode(vfo, mode, None).await?;
    } else {
        info!(
            "Setting {:.3} MHz with default USB mode",
            frequency as f64 / 1_000_000.0
        );
        
        rig.set_frequency(vfo, frequency).await?;
        rig.set_mode(vfo, Mode::USB, Some(2400)).await?;
    }

    Ok(())
}