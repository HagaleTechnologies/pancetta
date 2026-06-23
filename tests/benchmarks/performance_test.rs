//! Performance benchmarks for Pancetta components

use anyhow::Result;
use pancetta_ft8::{Ft8Decoder, Ft8Config};
use pancetta_dsp::{DspPipeline, ResamplingStage, BandpassFilter, NoiseReduction, Agc};
use std::time::{Duration, Instant};
use std::sync::Arc;

/// Benchmark FT8 decoder performance
#[tokio::test]
async fn benchmark_ft8_decoder() -> Result<()> {
    let config = Ft8Config::default();
    let decoder = Ft8Decoder::new(config);
    
    // Generate test signal (12.64 seconds at 12kHz)
    let samples = 12000.0 * 12.64;
    let test_signal: Vec<f32> = (0..samples as usize)
        .map(|i| {
            let t = i as f32 / 12000.0;
            // Simple test signal with multiple tones
            (2.0 * std::f32::consts::PI * 1000.0 * t).sin() * 0.1 +
            (2.0 * std::f32::consts::PI * 1500.0 * t).sin() * 0.1 +
            (2.0 * std::f32::consts::PI * 2000.0 * t).sin() * 0.1
        })
        .collect();
    
    // Warm up
    for _ in 0..3 {
        let _ = decoder.decode(&test_signal).await;
    }
    
    // Benchmark
    let iterations = 10;
    let start = Instant::now();
    
    for _ in 0..iterations {
        let _ = decoder.decode(&test_signal).await;
    }
    
    let elapsed = start.elapsed();
    let avg_time = elapsed / iterations;
    let decode_rate = 12.64 / avg_time.as_secs_f64();
    
    println!("FT8 Decoder Performance:");
    println!("  Iterations: {}", iterations);
    println!("  Total time: {:?}", elapsed);
    println!("  Average time per decode: {:?}", avg_time);
    println!("  Decode rate: {:.2}x real-time", decode_rate);
    
    // Should be able to decode faster than real-time
    assert!(decode_rate > 1.0, "FT8 decode rate {:.2}x is too slow", decode_rate);
    
    Ok(())
}

/// Benchmark DSP pipeline performance
#[tokio::test]
async fn benchmark_dsp_pipeline() -> Result<()> {
    // Create DSP pipeline
    let mut pipeline = DspPipeline::new();
    
    // Add processing stages
    pipeline.add_stage(Box::new(ResamplingStage::new(48000, 12000)));
    pipeline.add_stage(Box::new(BandpassFilter::new(200.0, 3000.0, 12000)));
    pipeline.add_stage(Box::new(NoiseReduction::new()));
    pipeline.add_stage(Box::new(Agc::new()));
    
    // Generate test audio (1 second at 48kHz)
    let test_audio: Vec<f32> = (0..48000)
        .map(|i| {
            let t = i as f32 / 48000.0;
            (2.0 * std::f32::consts::PI * 1000.0 * t).sin()
        })
        .collect();
    
    // Warm up
    for _ in 0..10 {
        let _ = pipeline.process(&test_audio);
    }
    
    // Benchmark
    let iterations = 100;
    let start = Instant::now();
    
    for _ in 0..iterations {
        let _ = pipeline.process(&test_audio);
    }
    
    let elapsed = start.elapsed();
    let avg_time = elapsed / iterations;
    let processing_rate = 1.0 / avg_time.as_secs_f64();
    
    println!("DSP Pipeline Performance:");
    println!("  Iterations: {}", iterations);
    println!("  Total time: {:?}", elapsed);
    println!("  Average time per second of audio: {:?}", avg_time);
    println!("  Processing rate: {:.2}x real-time", processing_rate);
    
    // Should process much faster than real-time
    assert!(processing_rate > 10.0, "DSP processing rate {:.2}x is too slow", processing_rate);
    
    Ok(())
}

/// Test CPU usage under load
#[tokio::test]
async fn test_cpu_usage() -> Result<()> {
    use sysinfo::{System, SystemExt, ProcessExt};
    use tokio::time::interval;
    
    let mut system = System::new();
    let pid = sysinfo::get_current_pid().unwrap();
    
    // Start heavy processing task
    let processing_handle = tokio::spawn(async move {
        let config = Ft8Config::default();
        let decoder = Ft8Decoder::new(config);
        
        // Generate test signal
        let samples = 12000.0 * 12.64;
        let test_signal: Vec<f32> = (0..samples as usize)
            .map(|i| (i as f32 * 0.001).sin())
            .collect();
        
        // Continuous decoding for 5 seconds
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(5) {
            let _ = decoder.decode(&test_signal).await;
        }
    });
    
    // Monitor CPU usage
    let mut cpu_samples = Vec::new();
    let mut monitor_interval = interval(Duration::from_millis(100));
    
    for _ in 0..50 {  // 5 seconds of monitoring
        monitor_interval.tick().await;
        system.refresh_process(pid);
        
        if let Some(process) = system.process(pid) {
            cpu_samples.push(process.cpu_usage());
        }
    }
    
    // Wait for processing to complete
    let _ = processing_handle.await;
    
    // Calculate average CPU usage
    let avg_cpu = cpu_samples.iter().sum::<f32>() / cpu_samples.len() as f32;
    let max_cpu = cpu_samples.iter().fold(0.0f32, |a, &b| a.max(b));
    
    println!("CPU Usage:");
    println!("  Samples: {}", cpu_samples.len());
    println!("  Average: {:.1}%", avg_cpu);
    println!("  Maximum: {:.1}%", max_cpu);
    
    // Verify CPU usage < 25% on average
    assert!(avg_cpu < 25.0, "Average CPU usage {:.1}% exceeds 25% target", avg_cpu);
    
    Ok(())
}

/// Benchmark simultaneous decode performance
#[tokio::test]
async fn benchmark_simultaneous_decodes() -> Result<()> {
    let config = Ft8Config::default();
    
    // Generate test signals with different frequencies
    let base_samples = 12000.0 * 12.64;
    let test_signals: Vec<Vec<f32>> = (0..50)
        .map(|signal_idx| {
            (0..base_samples as usize)
                .map(|i| {
                    let t = i as f32 / 12000.0;
                    let freq = 500.0 + (signal_idx as f32 * 50.0);
                    (2.0 * std::f32::consts::PI * freq * t).sin() * 0.1
                })
                .collect()
        })
        .collect();
    
    println!("Testing {} simultaneous decodes...", test_signals.len());
    
    let start = Instant::now();
    
    // Spawn all decode tasks
    let mut handles = Vec::new();
    for signal in test_signals {
        let config = config.clone();
        let handle = tokio::spawn(async move {
            let decoder = Ft8Decoder::new(config);
            decoder.decode(&signal).await
        });
        handles.push(handle);
    }
    
    // Wait for all to complete
    let mut successful = 0;
    for handle in handles {
        if handle.await.is_ok() {
            successful += 1;
        }
    }
    
    let elapsed = start.elapsed();
    let decode_rate = (successful as f64 * 12.64) / elapsed.as_secs_f64();
    
    println!("Simultaneous Decode Performance:");
    println!("  Signals: {}", successful);
    println!("  Total time: {:?}", elapsed);
    println!("  Effective decode rate: {:.2}x real-time", decode_rate);
    
    // Should handle 50+ simultaneous decodes
    assert!(successful >= 50, "Only completed {} simultaneous decodes", successful);
    
    Ok(())
}