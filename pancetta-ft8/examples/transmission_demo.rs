//! FT8 Transmission Demo
//!
//! This example demonstrates the complete FT8 transmission pipeline:
//! 1. Encoding various message types
//! 2. Audio modulation
//! 3. System testing and validation
//!
//! Run with: cargo run --example transmission_demo --features transmit

use pancetta_ft8::{
    convert_samples, AudioFormat, Ft8Encoder, Ft8Modulator, Ft8Transmitter, TransmissionConfig,
    MESSAGE_DURATION, SAMPLE_RATE,
};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("🚀 Pancetta FT8 Transmission Demo");
    println!("==================================\n");

    // Demonstrate message encoding
    demo_message_encoding()?;

    // Demonstrate audio modulation
    demo_audio_modulation()?;

    // Demonstrate complete transmission system
    demo_transmission_system()?;

    println!("✅ Demo completed successfully!");
    Ok(())
}

fn demo_message_encoding() -> Result<(), Box<dyn std::error::Error>> {
    println!("📡 FT8 Message Encoding Demo");
    println!("----------------------------");

    let mut encoder = Ft8Encoder::new();

    // Test various message types
    let messages = [
        ("CQ W1ABC FN42", "CQ call"),
        ("W1ABC K1DEF FN41", "Response"),
        ("K1DEF W1ABC -12", "Signal report"),
        ("K1DEF W1ABC RRR", "Acknowledgment"),
        ("K1DEF W1ABC 73", "Final"),
        ("HELLO WORLD", "Free text"),
    ];

    for (message, description) in messages {
        match encoder.encode_message(message, None) {
            Ok(symbols) => {
                println!(
                    "✅ {} '{}': {} symbols",
                    description,
                    message,
                    symbols.len()
                );
                print!("   Symbols: ");
                for (i, &symbol) in symbols.iter().take(10).enumerate() {
                    print!("{}", symbol);
                    if i < 9 {
                        print!(" ");
                    }
                }
                println!("...");
            }
            Err(e) => {
                println!("❌ Failed to encode '{}': {}", message, e);
            }
        }
    }

    println!();
    Ok(())
}

fn demo_audio_modulation() -> Result<(), Box<dyn std::error::Error>> {
    println!("🎵 Audio Modulation Demo");
    println!("------------------------");

    let mut encoder = Ft8Encoder::new();
    let mut modulator = Ft8Modulator::new_default()?;

    let config = modulator.get_config();
    println!("Modulator configuration:");
    println!("  Sample rate: {} Hz", config.sample_rate);
    println!("  Base frequency: {} Hz", config.base_frequency);
    println!("  TX power: {:.1}%", config.tx_power * 100.0);
    println!("  Tone spacing: {} Hz", config.tone_spacing);

    // Encode and modulate a test message
    let test_message = "CQ TEST W1ABC";
    let symbols = encoder.encode_message(test_message, None)?;
    let audio_samples = modulator.modulate_symbols(&symbols, 0.0)?;

    println!("\nAudio generation:");
    println!("  Message: '{}'", test_message);
    println!("  Symbols: {} (0-7 range)", symbols.len());
    println!("  Audio samples: {}", audio_samples.len());
    println!("  Duration: {:.2} seconds", MESSAGE_DURATION);
    println!("  Sample rate: {} Hz", SAMPLE_RATE);

    // Calculate some audio statistics
    let rms =
        (audio_samples.iter().map(|&s| s * s).sum::<f32>() / audio_samples.len() as f32).sqrt();
    let peak = audio_samples
        .iter()
        .map(|&s| s.abs())
        .fold(0.0f32, f32::max);

    println!("  RMS amplitude: {:.4}", rms);
    println!("  Peak amplitude: {:.4}", peak);

    // Test different audio formats
    println!("\nAudio format conversion:");
    let formats = [
        ("16-bit signed", AudioFormat::ft8_standard()),
        ("32-bit float", AudioFormat::ft8_high_quality()),
    ];

    for (name, format) in formats {
        let converted = convert_samples(&audio_samples, format);
        println!(
            "  {}: {} bytes ({} bytes/sample)",
            name,
            converted.len(),
            format.bytes_per_sample()
        );
    }

    println!();
    Ok(())
}

fn demo_transmission_system() -> Result<(), Box<dyn std::error::Error>> {
    println!("⚡ Transmission System Demo");
    println!("---------------------------");

    // Create transmission configuration
    let config = TransmissionConfig::default();
    println!("System configuration:");
    println!("  PTT method: {:?}", config.ptt_config.method);
    println!(
        "  Max TX time: {} seconds",
        config.safety_config.max_tx_time_seconds
    );
    println!(
        "  Band edges: {:.0} - {:.0} Hz",
        config.frequency_config.band_limits.lower_edge,
        config.frequency_config.band_limits.upper_edge
    );

    // Create transmitter (this doesn't require actual hardware)
    let transmitter = Ft8Transmitter::new(config)?;
    println!("✅ Transmitter created successfully");

    // Check initial state
    let state = transmitter.get_state();
    let stats = transmitter.get_statistics();
    println!("  State: {:?}", state);
    println!("  Transmission allowed: {}", stats.transmission_allowed);
    println!("  Total transmissions: {}", stats.total_transmissions);
    println!(
        "  Remaining TX time: {} seconds",
        stats.remaining_tx_time_seconds
    );

    // Test emergency stop functionality
    println!("\nTesting emergency stop:");
    transmitter.emergency_stop();
    let emergency_state = transmitter.get_state();
    println!("  Emergency state: {:?}", emergency_state);

    transmitter.clear_emergency_stop()?;
    let cleared_state = transmitter.get_state();
    println!("  Cleared state: {:?}", cleared_state);

    println!();
    Ok(())
}

// Additional demo functions for async features
#[cfg(feature = "transmit")]
#[tokio::main]
// rationale: demonstration helper retained for reference; not wired into `main`.
#[allow(dead_code)]
async fn async_demo() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔄 Async Transmission Demo");
    println!("---------------------------");

    let config = TransmissionConfig::default();
    let mut transmitter = Ft8Transmitter::new(config)?;

    // Run system test
    println!("Running transmission system test...");
    match transmitter.test_transmission_system(0.1).await {
        Ok(test_report) => {
            println!("✅ System test completed:");
            println!(
                "  PTT test: {}",
                if test_report.ptt_test.success {
                    "PASS"
                } else {
                    "FAIL"
                }
            );
            println!(
                "  Audio test: {}",
                if test_report.audio_test.success {
                    "PASS"
                } else {
                    "FAIL"
                }
            );
            println!(
                "  Frequency test: {}",
                if test_report.frequency_test.within_tolerance {
                    "PASS"
                } else {
                    "FAIL"
                }
            );
            println!("  Total time: {:?}", test_report.total_test_time);
        }
        Err(e) => {
            println!("❌ System test failed: {}", e);
        }
    }

    println!("\nNote: Actual transmission requires proper hardware setup");
    println!("This demo shows the software pipeline without transmitting RF.");

    Ok(())
}

// Helper function to demonstrate various encoding scenarios
// rationale: demonstration helper retained for reference; not wired into `main`.
#[allow(dead_code)]
fn demo_advanced_encoding() -> Result<(), Box<dyn std::error::Error>> {
    println!("🔬 Advanced Encoding Demo");
    println!("-------------------------");

    let mut encoder = Ft8Encoder::new();

    // Test specific encoding methods
    println!("Specific message encoders:");

    // CQ encoding
    let cq_symbols = encoder.encode_cq("W1ABC", "FN42", false)?;
    println!("  CQ: {} symbols", cq_symbols.len());

    // DX CQ encoding
    let dx_symbols = encoder.encode_cq("W1ABC", "FN42", true)?;
    println!("  DX CQ: {} symbols", dx_symbols.len());

    // Signal report with extreme values
    let weak_report = encoder.encode_signal_report("K1DEF", "W1ABC", -30)?;
    let _strong_report = encoder.encode_signal_report("K1DEF", "W1ABC", 20)?;
    println!("  Signal reports: {} symbols each", weak_report.len());

    // Free text variations
    let short_text = encoder.encode_freetext("73")?;
    let _long_text = encoder.encode_freetext("HELLO WORLD")?;
    println!("  Free text: {} symbols each", short_text.len());

    println!();
    Ok(())
}
