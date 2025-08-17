//! # FT8 DSP Pipeline Demo
//! 
//! This example demonstrates how to use the pancetta-dsp pipeline for FT8 signal processing.
//! It shows the complete flow from raw audio input to processed FT8 windows ready for decoding.

use pancetta_dsp::{
    factory, utils, DspPipeline, PipelineBuilder,
};
use std::time::Duration;
use tokio::time::sleep;
use tracing_subscriber::fmt::init;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    init();

    println!("🚀 Pancetta DSP - FT8 Pipeline Demo");
    println!("=====================================");

    // Create an FT8-optimized processing pipeline
    println!("📡 Creating FT8-optimized DSP pipeline...");
    let (mut pipeline, input_tx, output_rx) = factory::create_ft8_pipeline()?;

    println!("✅ Pipeline created successfully!");
    print_pipeline_info(&pipeline);

    // Generate some test audio data (simulating real-time audio input)
    println!("\n🎵 Generating test audio data...");
    let sample_rate = 48000.0;
    let duration_seconds = 1.0;
    let samples_per_buffer = 1024;
    let total_samples = (sample_rate * duration_seconds) as usize;
    
    // Create a synthetic FT8-like signal (simplified)
    let test_signal = generate_test_signal(sample_rate, duration_seconds);
    println!("Generated {} samples of test audio", test_signal.len());

    // Start the pipeline in a background task
    println!("\n⚡ Starting DSP pipeline...");
    let pipeline_handle = tokio::spawn(async move {
        match pipeline.start().await {
            Ok(_) => println!("Pipeline completed successfully"),
            Err(e) => eprintln!("Pipeline error: {}", e),
        }
    });

    // Simulate real-time audio streaming
    println!("📊 Streaming audio data through pipeline...");
    let mut samples_sent = 0;
    let mut windows_received = 0;

    // Send audio data in chunks
    for chunk in test_signal.chunks(samples_per_buffer) {
        if let Err(e) = input_tx.send(chunk.to_vec()) {
            eprintln!("Failed to send audio chunk: {}", e);
            break;
        }
        samples_sent += chunk.len();
        
        // Check for processed windows (non-blocking)
        while let Ok(window) = output_rx.try_recv() {
            windows_received += 1;
            println!("📦 Received FT8 window #{} ({} samples)", windows_received, window.len());
            
            // Analyze the window
            let rms = utils::calculate_rms(&window);
            let peak = utils::calculate_peak(&window);
            println!("   📈 RMS: {:.4}, Peak: {:.4}", rms, peak);
        }

        // Small delay to simulate real-time streaming
        sleep(Duration::from_millis(10)).await;
    }

    println!("\n📊 Processing Summary:");
    println!("   Samples sent: {}", samples_sent);
    println!("   Windows received: {}", windows_received);

    // Wait a bit for any remaining processing
    println!("\n⏳ Waiting for final processing...");
    sleep(Duration::from_millis(500)).await;

    // Check for any remaining windows
    while let Ok(window) = output_rx.try_recv() {
        windows_received += 1;
        println!("📦 Final window #{} ({} samples)", windows_received, window.len());
    }

    println!("\n✅ Demo completed!");
    println!("   Total windows processed: {}", windows_received);

    // The pipeline will stop when the main function ends
    drop(input_tx); // Close the input channel
    
    // Give the pipeline a moment to clean up
    sleep(Duration::from_millis(100)).await;

    Ok(())
}

fn print_pipeline_info(pipeline: &DspPipeline) {
    let stats = pipeline.stats();
    let (buffer_len, buffer_capacity, buffer_latency) = pipeline.buffer_status();
    
    println!("   🔧 Pipeline Configuration:");
    println!("      - Running: {}", pipeline.is_running());
    println!("      - Buffer: {}/{} samples ({:.2}ms latency)", 
             buffer_len, buffer_capacity, buffer_latency * 1000.0);
    println!("      - Processed frames: {}", stats.frames_processed);
}

fn generate_test_signal(sample_rate: f32, duration: f32) -> Vec<f32> {
    let num_samples = (sample_rate * duration) as usize;
    let mut signal = Vec::with_capacity(num_samples);
    
    // Generate a composite signal with multiple components typical of FT8
    for i in 0..num_samples {
        let t = i as f32 / sample_rate;
        
        // Base FT8-like multi-tone signal (simplified)
        let freq1 = 1000.0; // Hz
        let freq2 = 1500.0; // Hz  
        let freq3 = 2000.0; // Hz
        
        let sample = 0.1 * (2.0 * std::f32::consts::PI * freq1 * t).sin()
                   + 0.08 * (2.0 * std::f32::consts::PI * freq2 * t).sin()
                   + 0.06 * (2.0 * std::f32::consts::PI * freq3 * t).sin()
                   + 0.02 * (rand::random::<f32>() - 0.5); // Add some noise
        
        signal.push(sample);
    }
    
    signal
}

/// Example showing custom pipeline configuration
async fn custom_pipeline_example() -> Result<(), Box<dyn std::error::Error>> {
    println!("\n🔧 Custom Pipeline Configuration Example");
    
    let (_pipeline, _input_tx, _output_rx) = PipelineBuilder::new()
        .input_sample_rate(48000.0)
        .output_sample_rate(12000.0)
        .max_latency(0.3) // 300ms max latency
        .enable_agc(true)
        .enable_noise_reduction(false) // Disable NR for this example
        .enable_bandpass(true)
        .block_size(512) // Smaller blocks for lower latency
        .build()?;
    
    println!("✅ Custom pipeline created with 300ms latency and 512-sample blocks");
    
    Ok(())
}