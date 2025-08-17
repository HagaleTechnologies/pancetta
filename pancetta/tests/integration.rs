//! # Pancetta Integration Tests
//!
//! Comprehensive end-to-end tests for the Pancetta application.
//! These tests validate the complete audio processing pipeline from
//! input to FT8 decoded output.
//!
//! ## Test Categories
//!
//! - **Component Integration**: Individual component integration
//! - **Pipeline Flow**: End-to-end audio processing
//! - **Performance Tests**: Latency and throughput validation
//! - **Error Handling**: Failure scenarios and recovery
//! - **Configuration Tests**: Configuration loading and validation

use anyhow::Result;
use assert_cmd::Command;
use pancetta::coordinator::ApplicationCoordinator;
use pancetta::message_bus::{ComponentId, ComponentMessage, MessageBus, MessageType};
use pancetta::runtime::{PancettaRuntime, RuntimeConfig};
use pancetta_audio::{AudioTestConfig, AudioDeviceManager};
use pancetta_config::Config;
use pancetta_dsp::factory;
use pancetta_ft8::{Ft8Config, Ft8Decoder};
use predicates::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::time::{sleep, timeout};
use tracing::{info, warn};
use tracing_test::traced_test;

/// Test configuration for integration tests
struct TestConfig {
    /// Test duration in seconds
    pub duration: Duration,
    /// Sample rate for audio tests
    pub sample_rate: u32,
    /// Buffer size for audio processing
    pub buffer_size: usize,
    /// Enable real audio devices (may fail on CI)
    pub use_real_audio: bool,
    /// Test data directory
    pub test_data_dir: Option<std::path::PathBuf>,
}

impl Default for TestConfig {
    fn default() -> Self {
        Self {
            duration: Duration::from_secs(2),
            sample_rate: 48000,
            buffer_size: 1024,
            use_real_audio: false, // Disabled for CI compatibility
            test_data_dir: None,
        }
    }
}

#[tokio::test]
#[traced_test]
async fn test_application_startup_shutdown() -> Result<()> {
    info!("Testing application startup and shutdown");
    
    let config = Config::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    
    // Create coordinator in headless mode for testing
    let coordinator = ApplicationCoordinator::new(
        config,
        None,    // no audio device
        true,    // no_audio
        true,    // headless
        false,   // no metrics
        9090,    // metrics port
        shutdown.clone(),
    ).await?;
    
    // Start coordinator in background
    let coordinator_handle = tokio::spawn(async move {
        coordinator.run().await
    });
    
    // Let it run briefly
    sleep(Duration::from_millis(100)).await;
    
    // Signal shutdown
    shutdown.store(true, Ordering::Relaxed);
    
    // Wait for graceful shutdown
    let result = timeout(Duration::from_secs(5), coordinator_handle).await;
    assert!(result.is_ok(), "Coordinator should shutdown gracefully");
    
    info!("Application startup/shutdown test completed");
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn test_message_bus_integration() -> Result<()> {
    info!("Testing message bus integration");
    
    let bus = MessageBus::new(1000)?;
    
    // Create channels for audio → DSP → FT8 → TUI pipeline
    let (audio_tx, audio_rx) = bus.create_channel(ComponentId::Audio).await?;
    let (dsp_tx, dsp_rx) = bus.create_channel(ComponentId::Dsp).await?;
    let (ft8_tx, ft8_rx) = bus.create_channel(ComponentId::Ft8Decoder).await?;
    let (tui_tx, tui_rx) = bus.create_channel(ComponentId::Tui).await?;
    
    // Test audio data flow
    let audio_samples = vec![0.1f32; 1024];
    let audio_message = ComponentMessage::new(
        ComponentId::Audio,
        ComponentId::Dsp,
        MessageType::AudioData(audio_samples.clone()),
        Instant::now(),
    );
    
    bus.send_message(audio_message).await?;
    
    // Verify message received by DSP
    let received = timeout(Duration::from_millis(100), dsp_rx.recv()).await?;
    assert!(received.is_ok());
    
    if let MessageType::AudioData(samples) = received.unwrap().message_type {
        assert_eq!(samples.len(), 1024);
    } else {
        panic!("Expected AudioData message");
    }
    
    // Test broadcast functionality
    let control_message = ComponentMessage::new(
        ComponentId::Coordinator,
        ComponentId::Audio, // Will be broadcast to all
        MessageType::Control(pancetta::message_bus::ControlMessage::StatusRequest),
        Instant::now(),
    );
    
    bus.broadcast_message(control_message).await?;
    
    // Check component health
    let health = bus.get_component_health().await;
    assert!(health.len() >= 4); // Should have all created components
    
    info!("Message bus integration test completed");
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn test_dsp_pipeline_integration() -> Result<()> {
    info!("Testing DSP pipeline integration");
    
    // Create FT8-optimized pipeline
    let (mut pipeline, input_tx, output_rx) = factory::create_ft8_pipeline()?;
    
    // Start pipeline in background
    let pipeline_handle = tokio::spawn(async move {
        pipeline.start().await
    });
    
    // Generate test audio signal (simple sine wave at 1500 Hz - FT8 center frequency)
    let sample_rate = 48000.0;
    let frequency = 1500.0; // FT8 center frequency
    let duration = 0.1; // 100ms
    let samples_count = (sample_rate * duration) as usize;
    
    let mut test_samples = Vec::with_capacity(samples_count);
    for i in 0..samples_count {
        let t = i as f32 / sample_rate;
        let sample = 0.1 * (2.0 * std::f32::consts::PI * frequency * t).sin();
        test_samples.push(sample);
    }
    
    // Send test samples through pipeline
    input_tx.send(test_samples)?;
    
    // Wait for processed output
    let output = timeout(Duration::from_secs(1), output_rx.recv()).await;
    assert!(output.is_ok(), "Should receive processed audio within timeout");
    
    let processed_samples = output.unwrap()?;
    assert!(!processed_samples.is_empty(), "Processed samples should not be empty");
    
    // Basic validation: processed samples should be different from input
    // (due to resampling, filtering, etc.)
    info!("Received {} processed samples", processed_samples.len());
    
    // Clean shutdown
    drop(input_tx);
    
    let pipeline_result = timeout(Duration::from_secs(2), pipeline_handle).await;
    assert!(pipeline_result.is_ok(), "Pipeline should shutdown cleanly");
    
    info!("DSP pipeline integration test completed");
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn test_ft8_decoder_integration() -> Result<()> {
    info!("Testing FT8 decoder integration");
    
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config)?;
    
    // Generate test FT8 window (12.64 seconds at 12 kHz = 151,680 samples)
    let window_size = 151_680;
    let test_window: Vec<f32> = (0..window_size)
        .map(|i| {
            // Generate test signal with some structure that might resemble FT8
            let t = i as f32 / 12000.0;
            0.01 * (2.0 * std::f32::consts::PI * 1500.0 * t).sin() + 
            0.005 * (2.0 * std::f32::consts::PI * 1750.0 * t).sin()
        })
        .collect();
    
    // Attempt to decode (likely no valid FT8 in test signal, but should not crash)
    let decode_result = decoder.decode_window(&test_window);
    
    // Decoder should handle invalid/noise input gracefully
    match decode_result {
        Ok(messages) => {
            info!("Decoder returned {} messages from test signal", messages.len());
            // Usually should be 0 for random test signal
        }
        Err(e) => {
            warn!("Decoder error on test signal (expected): {}", e);
            // This is acceptable for test noise
        }
    }
    
    info!("FT8 decoder integration test completed");
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn test_runtime_performance() -> Result<()> {
    info!("Testing runtime performance characteristics");
    
    let config = RuntimeConfig {
        worker_threads: 2,
        enable_metrics: true,
        ..Default::default()
    };
    
    let runtime = PancettaRuntime::new(config)?;
    runtime.start_metrics_collection().await?;
    
    // Spawn multiple tasks to test performance
    let task_count = 100;
    let mut handles = Vec::new();
    
    let start_time = Instant::now();
    
    for i in 0..task_count {
        let handle = runtime.spawn_critical(&format!("perf_test_{}", i), async move {
            // Simulate short real-time task
            tokio::task::yield_now().await;
            i
        });
        handles.push(handle);
    }
    
    // Wait for all tasks to complete
    let mut results = Vec::new();
    for handle in handles {
        let result = handle.await?;
        results.push(result);
    }
    
    let total_time = start_time.elapsed();
    
    // Verify all tasks completed
    assert_eq!(results.len(), task_count);
    for i in 0..task_count {
        assert_eq!(results[i], i);
    }
    
    // Performance assertions
    let avg_time_per_task = total_time / task_count as u32;
    assert!(avg_time_per_task < Duration::from_millis(10), 
            "Average task time should be <10ms, got {:?}", avg_time_per_task);
    
    // Check runtime health
    assert!(runtime.is_healthy().await, "Runtime should be healthy after task execution");
    
    // Get and validate metrics
    let metrics = runtime.get_metrics().await;
    assert!(metrics.tasks_executed >= task_count as u64);
    assert!(metrics.uptime > Duration::from_millis(1));
    
    info!("Runtime performance test completed: {} tasks in {:?}", task_count, total_time);
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn test_configuration_loading() -> Result<()> {
    info!("Testing configuration loading and validation");
    
    // Test default configuration
    let default_config = Config::default();
    assert!(default_config.validate().is_ok(), "Default config should be valid");
    
    // Test configuration serialization/deserialization
    let temp_dir = TempDir::new()?;
    let config_path = temp_dir.path().join("test_config.toml");
    
    default_config.save_to_file(&config_path)?;
    assert!(config_path.exists(), "Configuration file should be created");
    
    let loaded_config = Config::load_from_file(&config_path)?;
    assert!(loaded_config.validate().is_ok(), "Loaded config should be valid");
    
    // Verify some key fields are preserved
    assert_eq!(default_config.station.callsign, loaded_config.station.callsign);
    assert_eq!(default_config.audio.input_device, loaded_config.audio.input_device);
    
    info!("Configuration loading test completed");
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn test_error_handling() -> Result<()> {
    info!("Testing error handling and recovery");
    
    let bus = MessageBus::new(10)?; // Small buffer to test overflow
    let (tx, rx) = bus.create_channel(ComponentId::Audio).await?;
    
    // Test channel overflow handling
    for i in 0..20 {
        let message = ComponentMessage::new(
            ComponentId::Audio,
            ComponentId::Dsp,
            MessageType::AudioData(vec![i as f32; 100]),
            Instant::now(),
        );
        
        // Some messages should be dropped due to small buffer
        let _ = bus.send_message(message).await;
    }
    
    // Bus should still be functional
    let stats = bus.get_statistics();
    assert!(stats.total_messages > 0);
    // Some messages may have been dropped due to buffer overflow
    
    // Test component health monitoring
    bus.update_heartbeat(ComponentId::Audio).await?;
    let health = bus.get_component_health().await;
    assert!(!health.is_empty());
    
    info!("Error handling test completed");
    Ok(())
}

#[test]
fn test_cli_interface() -> Result<()> {
    info!("Testing CLI interface");
    
    // Test help command
    let mut cmd = Command::cargo_bin("pancetta")?;
    cmd.arg("--help");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("High-performance amateur radio"));
    
    // Test version command
    let mut cmd = Command::cargo_bin("pancetta")?;
    cmd.arg("--version");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));
    
    // Test config validation
    let mut cmd = Command::cargo_bin("pancetta")?;
    cmd.args(&["config", "--validate"]);
    cmd.assert().success(); // Should validate default config
    
    // Test info command
    let mut cmd = Command::cargo_bin("pancetta")?;
    cmd.arg("info");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("Pancetta System Information"));
    
    info!("CLI interface test completed");
    Ok(())
}

#[cfg(feature = "audio-device-tests")]
#[tokio::test]
#[traced_test]
async fn test_audio_device_integration() -> Result<()> {
    info!("Testing audio device integration (requires audio hardware)");
    
    let device_manager = AudioDeviceManager::new()?;
    let devices = device_manager.list_devices()?;
    
    if devices.is_empty() {
        warn!("No audio devices found, skipping audio device test");
        return Ok(());
    }
    
    info!("Found {} audio devices", devices.len());
    for (i, device) in devices.iter().enumerate() {
        info!("  {}: {} ({})", i, device.name, device.driver);
    }
    
    // Test first available device
    let test_config = AudioTestConfig {
        device_name: Some(devices[0].name.clone()),
        duration_seconds: 1, // Short test
        sample_rate: 48000,
        buffer_size: 1024,
    };
    
    let test_result = device_manager.test_device(test_config).await?;
    
    assert!(test_result.success, "Audio device test should succeed");
    assert!(test_result.latency_ms < 100.0, "Latency should be reasonable");
    assert_eq!(test_result.sample_rate, 48000);
    
    info!("Audio device test completed successfully");
    info!("  Device: {}", test_result.device_name);
    info!("  Latency: {:.2}ms", test_result.latency_ms);
    info!("  Dropouts: {}", test_result.dropout_count);
    
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn test_end_to_end_pipeline() -> Result<()> {
    info!("Testing complete end-to-end processing pipeline");
    
    let test_config = TestConfig::default();
    
    // Create message bus
    let bus = MessageBus::new(10000)?;
    
    // Create component channels
    let (audio_tx, audio_rx) = bus.create_channel(ComponentId::Audio).await?;
    let (dsp_tx, dsp_rx) = bus.create_channel(ComponentId::Dsp).await?;
    let (ft8_tx, ft8_rx) = bus.create_channel(ComponentId::Ft8Decoder).await?;
    let (tui_tx, tui_rx) = bus.create_channel(ComponentId::Tui).await?;
    
    // Set up DSP pipeline
    let (mut dsp_pipeline, dsp_input_tx, dsp_output_rx) = factory::create_ft8_pipeline()?;
    
    // Set up FT8 decoder
    let ft8_config = Ft8Config::default();
    let mut ft8_decoder = Ft8Decoder::new(ft8_config)?;
    
    // Start DSP pipeline
    let dsp_handle = tokio::spawn(async move {
        dsp_pipeline.start().await
    });
    
    // Start message processing tasks
    let message_processor = tokio::spawn(async move {
        // Process DSP messages
        while let Ok(message) = dsp_rx.recv().await {
            if let MessageType::AudioData(samples) = message.message_type {
                if let Err(e) = dsp_input_tx.send(samples) {
                    warn!("Failed to send to DSP: {}", e);
                    break;
                }
            }
        }
    });
    
    let ft8_processor = tokio::spawn(async move {
        // Process FT8 decoder output
        while let Ok(processed_window) = dsp_output_rx.recv() {
            match ft8_decoder.decode_window(&processed_window) {
                Ok(messages) => {
                    for decoded_msg in messages {
                        let message = ComponentMessage::new(
                            ComponentId::Ft8Decoder,
                            ComponentId::Tui,
                            MessageType::DecodedMessage(decoded_msg),
                            Instant::now(),
                        );
                        
                        if let Err(e) = bus.send_message(message).await {
                            warn!("Failed to send decoded message: {}", e);
                        }
                    }
                }
                Err(e) => {
                    // Expected for test signals
                    // debug!("FT8 decode error: {}", e);
                }
            }
        }
    });
    
    // Generate test audio signal
    let sample_rate = test_config.sample_rate as f32;
    let samples_count = (sample_rate * test_config.duration.as_secs_f32()) as usize;
    
    let test_audio = (0..samples_count)
        .map(|i| {
            let t = i as f32 / sample_rate;
            // Multi-tone signal that might trigger some DSP processing
            0.05 * (2.0 * std::f32::consts::PI * 1000.0 * t).sin() +
            0.03 * (2.0 * std::f32::consts::PI * 1500.0 * t).sin() +
            0.02 * (2.0 * std::f32::consts::PI * 2000.0 * t).sin()
        })
        .collect::<Vec<f32>>();
    
    // Send audio data through pipeline in chunks
    let chunk_size = test_config.buffer_size;
    for chunk in test_audio.chunks(chunk_size) {
        let audio_message = ComponentMessage::new(
            ComponentId::Audio,
            ComponentId::Dsp,
            MessageType::AudioData(chunk.to_vec()),
            Instant::now(),
        );
        
        bus.send_message(audio_message).await?;
        
        // Small delay to simulate real-time processing
        sleep(Duration::from_millis(1)).await;
    }
    
    // Let processing complete
    sleep(Duration::from_millis(500)).await;
    
    // Check for any decoded messages (probably none for test signal)
    let mut message_count = 0;
    while let Ok(_message) = tui_rx.try_recv() {
        message_count += 1;
    }
    
    info!("End-to-end pipeline processed {} messages", message_count);
    
    // Verify pipeline is still healthy
    let stats = bus.get_statistics();
    assert!(stats.total_messages > 0, "Should have processed some messages");
    
    // Clean shutdown
    drop(dsp_input_tx);
    let _ = timeout(Duration::from_secs(2), dsp_handle).await;
    let _ = timeout(Duration::from_secs(1), message_processor).await;
    let _ = timeout(Duration::from_secs(1), ft8_processor).await;
    
    info!("End-to-end pipeline test completed successfully");
    Ok(())
}

#[tokio::test]
#[traced_test]
async fn test_performance_benchmarks() -> Result<()> {
    info!("Running performance benchmarks");
    
    let iterations = 10;
    let mut latencies = Vec::new();
    
    // Benchmark message bus latency
    let bus = MessageBus::new(10000)?;
    let (tx, rx) = bus.create_channel(ComponentId::Audio).await?;
    
    for i in 0..iterations {
        let start = Instant::now();
        
        let message = ComponentMessage::new(
            ComponentId::Audio,
            ComponentId::Dsp,
            MessageType::AudioData(vec![i as f32; 1024]),
            Instant::now(),
        );
        
        bus.send_message(message).await?;
        let _ = rx.recv().await?;
        
        let latency = start.elapsed();
        latencies.push(latency);
    }
    
    // Calculate statistics
    let avg_latency: Duration = latencies.iter().sum::<Duration>() / latencies.len() as u32;
    let max_latency = latencies.iter().max().unwrap();
    let min_latency = latencies.iter().min().unwrap();
    
    info!("Message bus latency benchmarks:");
    info!("  Average: {:?}", avg_latency);
    info!("  Min: {:?}", min_latency);
    info!("  Max: {:?}", max_latency);
    
    // Performance assertions
    assert!(avg_latency < Duration::from_millis(1), 
            "Average message latency should be <1ms, got {:?}", avg_latency);
    assert!(max_latency < Duration::from_millis(5), 
            "Max message latency should be <5ms, got {:?}", max_latency);
    
    info!("Performance benchmark completed successfully");
    Ok(())
}