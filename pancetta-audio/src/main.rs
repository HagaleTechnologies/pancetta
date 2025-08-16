//! Pancetta Week 0 Technical POC - Real-Time Audio Latency Test
//! 
//! This is the critical proof-of-concept that determines if the Pancetta project
//! architecture is viable. We must prove <1ms audio callback latency.
//! 
//! If this test fails, the entire project needs architectural changes.

use pancetta_audio::{AudioConfig, RealtimeAudioProcessor};
use std::io;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🎯 Pancetta Week 0 Technical POC - Real-Time Audio Latency Test");
    println!("================================================================");
    println!("CRITICAL: Must prove <1ms audio callback latency for project viability\n");
    
    // Initialize audio configuration for ultra-low latency
    let config = AudioConfig {
        sample_rate: 48000,    // Professional audio standard
        buffer_size: 64,       // Ultra-low latency: 64 samples = 1.33ms at 48kHz
        input_channels: 2,     // Stereo input
        output_channels: 2,    // Stereo output
    };
    
    println!("Audio Configuration:");
    println!("• Sample Rate: {}Hz", config.sample_rate);
    println!("• Buffer Size: {} samples", config.buffer_size);
    println!("• Channels: {} in, {} out", config.input_channels, config.output_channels);
    println!("• Theoretical Min Latency: {:.3}ms\n", 
             (config.buffer_size as f64 / config.sample_rate as f64) * 1000.0);
    
    // Create the real-time audio processor
    let mut processor = match RealtimeAudioProcessor::new(config) {
        Ok(p) => {
            println!("✅ Audio processor initialized successfully");
            p
        }
        Err(e) => {
            println!("❌ Failed to initialize audio processor: {}", e);
            return Err(e);
        }
    };
    
    println!("\nPress Enter to start the latency stress test...");
    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    
    // Run the critical latency test
    println!("Starting real-time audio processing...");
    println!("Generating 1kHz test tone with latency measurement\n");
    
    match pancetta_audio::run_latency_stress_test(&mut processor, 30) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use pancetta_audio::latency::{LatencyMeasurer, CallbackTimer};
    
    #[test]
    fn test_latency_measurer() {
        let mut measurer = LatencyMeasurer::new(100, 1_000_000); // 1ms target
        
        // Add some test measurements
        measurer.record_latency(500_000);   // 0.5ms - good
        measurer.record_latency(800_000);   // 0.8ms - good
        measurer.record_latency(1_200_000); // 1.2ms - excessive
        measurer.record_latency(700_000);   // 0.7ms - good
        
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
        assert!(elapsed_ns > 500_000);  // At least 0.5ms
        assert!(elapsed_ns < 2_000_000); // At most 2ms
        assert!(elapsed_ms > 0.5);
        assert!(elapsed_ms < 2.0);
    }
    
    #[test]
    fn test_audio_config_defaults() {
        let config = AudioConfig::default();
        assert_eq!(config.sample_rate, 48000);
        assert_eq!(config.buffer_size, 64);
        assert_eq!(config.input_channels, 2);
        assert_eq!(config.output_channels, 2);
    }
    
    #[test]
    fn test_theoretical_latency_calculation() {
        let config = AudioConfig::default();
        let processor = RealtimeAudioProcessor::new(config);
        
        // This test may fail on systems without audio devices
        if let Ok(proc) = processor {
            let latency = proc.theoretical_min_latency_ms();
            // 64 samples at 48kHz = 1.333ms
            assert!((latency - 1.333).abs() < 0.001);
        }
    }
}