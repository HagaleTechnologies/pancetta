//! Pancetta Week 0 Technical POC - Real-Time Audio Latency Test
//!
//! This is the critical proof-of-concept that determines if the Pancetta project
//! architecture is viable. We must prove <1ms audio callback latency.
//!
//! If this test fails, the entire project needs architectural changes.

use pancetta_audio::{AudioDeviceManager, AudioProcessor, AudioProcessorConfig};
use std::io;
use std::time::Duration;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🎯 Pancetta Week 0 Technical POC - Real-Time Audio Latency Test");
    println!("================================================================");
    println!("CRITICAL: Must prove <1ms audio callback latency for project viability\n");

    // First, enumerate available audio devices
    println!("Available Audio Devices:");
    let device_manager = AudioDeviceManager::new()?;
    for device in device_manager.list_device_info() {
        println!("  {}", device);
    }

    println!("\nFT8-Compatible Devices:");
    for device in device_manager.find_ft8_compatible_devices() {
        println!(
            "  {} (supports: {}Hz)",
            device.name,
            device
                .input_sample_rates
                .iter()
                .map(|r| r.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Initialize audio processor for ultra-low latency
    let config = AudioProcessorConfig::for_ft8();

    println!("\nAudio Configuration:");
    println!("• Sample Rate: {}Hz", config.stream_config.sample_rate);
    println!(
        "• Buffer Size: {} samples",
        config.stream_config.buffer_size
    );
    println!(
        "• Channels: {} in, {} out",
        config.stream_config.input_channels, config.stream_config.output_channels
    );
    println!("• Target Rate: {}Hz (FT8)", config.target_sample_rate);
    println!(
        "• Theoretical Min Latency: {:.3}ms\n",
        config.stream_config.theoretical_latency_ms()
    );

    // Create the real-time audio processor
    let mut processor = match AudioProcessor::new(config).await {
        Ok(p) => {
            println!("✅ Audio processor initialized successfully");
            p
        }
        Err(e) => {
            println!("❌ Failed to initialize audio processor: {}", e);
            return Err(e.into());
        }
    };

    println!("\nPress Enter to start the latency stress test...");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    // Run the critical latency test
    println!("Starting real-time audio processing...");
    println!("Testing with actual audio input/output streams\n");

    match run_audio_latency_test(&mut processor, 30).await {
        Ok(()) => {
            println!("\n🎉 WEEK 0 POC SUCCESSFUL!");
            println!("The Pancetta real-time audio architecture is VIABLE.");
            println!("Proceeding to Week 1 development is APPROVED.");
        }
        Err(e) => {
            println!("\n💥 WEEK 0 POC FAILED!");
            println!("Error: {}", e);
            println!("The Pancetta architecture requires fundamental changes.");
            println!("❌ CANNOT proceed to Week 1 development.");
            std::process::exit(1);
        }
    }

    println!("\nPress Enter to exit...");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;

    Ok(())
}

/// Run the audio latency test using the new processor
async fn run_audio_latency_test(
    processor: &mut AudioProcessor,
    test_duration_seconds: u64,
) -> Result<(), Box<dyn std::error::Error>> {
    use pancetta_audio::latency::LatencyMeasurer;

    println!("Starting audio processor...");
    processor.start().await?;

    let mut latency_measurer = LatencyMeasurer::new(1000, 1_000_000); // 1ms target
    let mut last_stats_time = std::time::Instant::now();
    let test_start = std::time::Instant::now();

    println!(
        "Collecting samples and measuring latency for {} seconds...",
        test_duration_seconds
    );

    while test_start.elapsed().as_secs() < test_duration_seconds {
        // Process any available samples
        let samples = processor.get_processed_samples().await?;

        if !samples.is_empty() {
            println!("Processed {} audio samples", samples.len());

            // Calculate processing latency for each sample
            for sample in samples {
                let latency_ns = sample.timestamp.elapsed().as_nanos() as u64;
                latency_measurer.record_latency(latency_ns);
            }
        }

        // Print statistics every 5 seconds
        if last_stats_time.elapsed().as_secs() >= 5 && latency_measurer.measurement_count() > 0 {
            let stats = latency_measurer.get_stats();
            let proc_stats = processor.get_statistics().await;

            println!(
                "Progress: {}s - Avg Latency: {:.3}ms, Max: {:.3}ms, Samples: {}, Drops: {:.1}%",
                test_start.elapsed().as_secs(),
                stats.average_ms,
                stats.max_ms,
                proc_stats.stream_stats.samples_processed,
                proc_stats.stream_stats.drop_rate_percent
            );

            last_stats_time = std::time::Instant::now();
        }

        // Small sleep to prevent busy loop
        sleep(Duration::from_millis(100)).await;
    }

    // Stop processor
    processor.stop().await?;

    // Display final results
    let final_stats = latency_measurer.get_stats();
    let proc_stats = processor.get_statistics().await;

    println!("\n{}", final_stats.format_for_display());
    println!("Stream Statistics:");
    println!(
        "  Samples Processed: {}",
        proc_stats.stream_stats.samples_processed
    );
    println!(
        "  Samples Dropped: {}",
        proc_stats.stream_stats.samples_dropped
    );
    println!(
        "  Drop Rate: {:.2}%",
        proc_stats.stream_stats.drop_rate_percent
    );
    println!(
        "  Stream Health: {}",
        proc_stats.stream_stats.status_description()
    );

    // Validate if we meet the requirements
    let success = final_stats.meeting_target
        && proc_stats.stream_stats.drop_rate_percent < 1.0
        && proc_stats.stream_stats.samples_processed > 0;

    if success {
        println!("\n✅ SUCCESS: Audio system meets all requirements!");
        println!("   - Latency consistently <1ms");
        println!("   - Drop rate <1%");
        println!("   - Stream processing healthy");
        println!("   The Pancetta real-time architecture is VIABLE.");
    } else {
        println!("\n❌ FAILURE: Audio system does not meet requirements.");
        if !final_stats.meeting_target {
            println!("   - Latency exceeds 1ms target");
        }
        if proc_stats.stream_stats.drop_rate_percent >= 1.0 {
            println!(
                "   - Sample drop rate too high: {:.1}%",
                proc_stats.stream_stats.drop_rate_percent
            );
        }
        if proc_stats.stream_stats.samples_processed == 0 {
            println!("   - No audio samples processed (device issue?)");
        }
        println!("   The Pancetta architecture needs fundamental changes.");
        return Err("Audio requirements not met".into());
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_audio::latency::{CallbackTimer, LatencyMeasurer};

    #[test]
    fn test_latency_measurer() {
        let mut measurer = LatencyMeasurer::new(100, 1_000_000); // 1ms target

        // Add some test measurements
        measurer.record_latency(500_000); // 0.5ms - good
        measurer.record_latency(800_000); // 0.8ms - good
        measurer.record_latency(1_200_000); // 1.2ms - excessive
        measurer.record_latency(700_000); // 0.7ms - good

        let stats = measurer.get_stats();
        assert_eq!(stats.count, 4);
        assert_eq!(stats.excessive_percentage, 25.0); // 1 out of 4
        assert!(!stats.meeting_target); // >1% excessive
    }

    #[test]
    fn test_callback_timer() {
        let timer = CallbackTimer::start();
        std::thread::sleep(std::time::Duration::from_millis(1));
        let elapsed_ns = timer.elapsed_ns();
        let elapsed_ms = timer.elapsed_ms();

        // Should be approximately 1ms
        assert!(elapsed_ns > 500_000); // At least 0.5ms
        assert!(elapsed_ns < 2_000_000); // At most 2ms
        assert!(elapsed_ms > 0.5);
        assert!(elapsed_ms < 2.0);
    }

    #[test]
    fn test_audio_config_defaults() {
        let config = AudioProcessorConfig::default();
        assert_eq!(config.stream_config.sample_rate, 48000); // Uses 48kHz for compatibility
        assert_eq!(config.target_sample_rate, 12000); // Converts to 12kHz for FT8
        assert_eq!(config.stream_config.buffer_size, 64);
        assert_eq!(config.stream_config.input_channels, 1);
        assert_eq!(config.stream_config.output_channels, 2);
    }

    #[tokio::test]
    async fn test_theoretical_latency_calculation() {
        let config = AudioProcessorConfig::default();
        let processor = AudioProcessor::new(config).await;

        // This test may fail on systems without audio devices
        if let Ok(proc) = processor {
            let config = proc.get_config();
            let latency = config.stream_config.theoretical_latency_ms();
            // 64 samples at 48kHz = 1.333ms
            assert!((latency - 1.333).abs() < 0.001);
        }
    }
}
