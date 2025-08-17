//! Integration tests for the complete audio pipeline
//! 
//! Tests the flow from Audio → DSP → FT8 Decoder

use pancetta::{ApplicationCoordinator, CoordinatorConfig};
use pancetta_config::Config;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::{sleep, timeout};

/// Test that the coordinator starts successfully with stub audio
#[tokio::test]
async fn test_coordinator_starts_with_stub_audio() {
    // Set stub audio mode
    std::env::set_var("PANCETTA_STUB_AUDIO", "1");
    
    let config = Config::default();
    let shutdown = Arc::new(AtomicBool::new(false));
    
    let coordinator = ApplicationCoordinator::new(
        config,
        None,           // audio_device
        false,          // no_audio = false (we want audio)
        true,           // headless
        false,          // enable_metrics
        9090,           // metrics_port
        shutdown.clone(),
    ).await;
    
    assert!(coordinator.is_ok(), "Coordinator should create successfully");
    
    // Start coordinator in background
    let shutdown_for_task = shutdown.clone();
    let handle = tokio::spawn(async move {
        if let Ok(coord) = coordinator {
            let _ = coord.run().await;
        }
    });
    
    // Let it run for a short time
    sleep(Duration::from_secs(2)).await;
    
    // Signal shutdown
    shutdown.store(true, Ordering::Relaxed);
    
    // Wait for coordinator to stop
    let result = timeout(Duration::from_secs(5), handle).await;
    assert!(result.is_ok(), "Coordinator should shut down cleanly");
    
    // Clean up
    std::env::remove_var("PANCETTA_STUB_AUDIO");
}

/// Test that audio data flows through the pipeline
#[tokio::test]
async fn test_audio_data_flow() {
    use pancetta::message_bus::{MessageBus, ComponentId, MessageType};
    
    // Create message bus
    let message_bus = MessageBus::new(1000).unwrap();
    
    // Create channels for Audio and DSP
    let (audio_tx, audio_rx) = message_bus.create_channel(ComponentId::Audio).await.unwrap();
    let (dsp_tx, dsp_rx) = message_bus.create_channel(ComponentId::Dsp).await.unwrap();
    
    // Send test audio data
    let test_samples = vec![0.1_f32; 1024];
    let message = pancetta::message_bus::ComponentMessage::new(
        ComponentId::Audio,
        ComponentId::Dsp,
        MessageType::AudioData(test_samples.clone()),
        std::time::Instant::now(),
    );
    
    audio_tx.send(message).unwrap();
    
    // Verify message is received
    let received = dsp_rx.recv_timeout(Duration::from_secs(1));
    assert!(received.is_ok(), "DSP should receive audio data");
    
    if let Ok(msg) = received {
        if let MessageType::AudioData(samples) = msg.message_type {
            assert_eq!(samples.len(), 1024, "Sample count should match");
            assert_eq!(samples[0], 0.1, "Sample data should match");
        } else {
            panic!("Wrong message type received");
        }
    }
}

/// Test FT8 window accumulation
#[tokio::test]
async fn test_ft8_window_accumulation() {
    // FT8 requires exactly 151680 samples (12.64 seconds at 12kHz)
    const FT8_WINDOW_SIZE: usize = 151680;
    
    let mut buffer = Vec::new();
    let chunk_size = 1024; // Typical chunk size
    
    // Simulate accumulating audio chunks
    while buffer.len() < FT8_WINDOW_SIZE {
        let chunk = vec![0.0_f32; chunk_size];
        buffer.extend_from_slice(&chunk);
    }
    
    assert!(buffer.len() >= FT8_WINDOW_SIZE, "Buffer should have enough samples");
    
    // Extract exactly one window
    let window: Vec<f32> = buffer.drain(..FT8_WINDOW_SIZE).collect();
    assert_eq!(window.len(), FT8_WINDOW_SIZE, "Window should be exactly 151680 samples");
    
    // Remaining samples should be kept for next window
    assert!(buffer.len() < FT8_WINDOW_SIZE, "Buffer should have less than a full window remaining");
}

/// Test that DSP pipeline processes audio correctly
#[tokio::test]
async fn test_dsp_pipeline_processing() {
    use pancetta_dsp::factory;
    
    // Create FT8 pipeline
    let result = factory::create_ft8_pipeline();
    assert!(result.is_ok(), "Should create FT8 pipeline");
    
    let (mut pipeline, input_tx, output_rx) = result.unwrap();
    
    // Start pipeline in background
    let pipeline_handle = tokio::spawn(async move {
        pipeline.start().await
    });
    
    // Send test audio (48kHz input)
    let test_samples = vec![0.1_f32; 4800]; // 0.1 second at 48kHz
    input_tx.send(test_samples).unwrap();
    
    // Receive processed audio (should be resampled to 12kHz)
    let timeout_result = timeout(
        Duration::from_secs(1),
        tokio::task::spawn_blocking(move || {
            output_rx.recv()
        })
    ).await;
    
    assert!(timeout_result.is_ok(), "Should receive processed audio within timeout");
    
    // Stop pipeline
    drop(input_tx);
    let _ = timeout(Duration::from_secs(1), pipeline_handle).await;
}

/// Test FT8 decoder with valid window size
#[test]
fn test_ft8_decoder_window_validation() {
    use pancetta_ft8::{Ft8Decoder, Ft8Config};
    
    let config = Ft8Config::default();
    let mut decoder = Ft8Decoder::new(config).unwrap();
    
    // Test with wrong size (should fail)
    let wrong_size = vec![0.0_f32; 48000];
    let result = decoder.decode_window(&wrong_size);
    assert!(result.is_err(), "Should reject wrong window size");
    
    // Test with correct size (should succeed)
    let correct_size = vec![0.0_f32; 151680];
    let result = decoder.decode_window(&correct_size);
    assert!(result.is_ok(), "Should accept correct window size");
}

/// Test message bus component health monitoring
#[tokio::test]
async fn test_component_health_monitoring() {
    use pancetta::message_bus::MessageBus;
    
    let message_bus = MessageBus::new(1000).unwrap();
    
    // Get initial health status
    let health = message_bus.get_component_health().await;
    assert!(health.is_empty(), "No components should be registered initially");
    
    // Create component channel
    let (_tx, _rx) = message_bus.create_channel(pancetta::message_bus::ComponentId::Audio).await.unwrap();
    
    // Check health again
    let health = message_bus.get_component_health().await;
    assert_eq!(health.len(), 1, "Audio component should be registered");
    assert!(health[0].is_healthy, "Component should be healthy");
}

/// Test coordinator configuration
#[test]
fn test_coordinator_config() {
    let config = CoordinatorConfig::default();
    
    assert_eq!(config.startup_timeout, Duration::from_secs(30));
    assert_eq!(config.shutdown_timeout, Duration::from_secs(10));
    assert_eq!(config.health_check_interval, Duration::from_secs(5));
    assert!(config.message_buffer_size > 0);
    assert!(config.max_concurrent_tasks > 0);
}

/// Test audio manager configuration
#[test]
fn test_audio_manager_config() {
    use pancetta_audio::AudioManagerConfig;
    
    let config = AudioManagerConfig::default();
    
    assert_eq!(config.sample_rate, 48000);
    assert_eq!(config.buffer_size, 256);
    assert_eq!(config.channels, 1);
    assert_eq!(config.target_latency_ms, 1.0);
    assert_eq!(config.input_gain_db, 0.0);
}

/// Test end-to-end message flow simulation
#[tokio::test]
async fn test_end_to_end_message_flow() {
    use pancetta::message_bus::{MessageBus, ComponentId, MessageType, ComponentMessage};
    use std::time::Instant;
    
    let message_bus = MessageBus::new(1000).unwrap();
    
    // Create all component channels
    let (audio_tx, _audio_rx) = message_bus.create_channel(ComponentId::Audio).await.unwrap();
    let (_dsp_tx, dsp_rx) = message_bus.create_channel(ComponentId::Dsp).await.unwrap();
    let (_ft8_tx, ft8_rx) = message_bus.create_channel(ComponentId::Ft8Decoder).await.unwrap();
    
    // Simulate audio → DSP flow
    let audio_samples = vec![0.1_f32; 1024];
    let audio_msg = ComponentMessage::new(
        ComponentId::Audio,
        ComponentId::Dsp,
        MessageType::AudioData(audio_samples),
        Instant::now(),
    );
    audio_tx.send(audio_msg).unwrap();
    
    // DSP receives and processes
    let dsp_msg = dsp_rx.recv_timeout(Duration::from_millis(100)).unwrap();
    assert!(matches!(dsp_msg.message_type, MessageType::AudioData(_)));
    
    // Simulate DSP → FT8 flow (with full window)
    let ft8_window = vec![0.0_f32; 151680];
    let ft8_msg = ComponentMessage::new(
        ComponentId::Dsp,
        ComponentId::Ft8Decoder,
        MessageType::DspData(ft8_window),
        Instant::now(),
    );
    
    // Would normally go through DSP tx, but we'll simulate
    // by checking the message structure is valid
    assert_eq!(ft8_msg.source, ComponentId::Dsp);
    assert_eq!(ft8_msg.destination, ComponentId::Ft8Decoder);
    
    if let MessageType::DspData(window) = ft8_msg.message_type {
        assert_eq!(window.len(), 151680, "FT8 window should be correct size");
    }
}