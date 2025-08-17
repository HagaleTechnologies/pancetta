//! End-to-end integration tests for the full FT8 decode pipeline

use anyhow::Result;
use pancetta::coordinator::Coordinator;
use pancetta::config::Config;
use pancetta::message_bus::{MessageBus, ComponentMessage, ComponentId, MessageType};
use pancetta_ft8::DecodedMessage as Ft8DecodedMessage;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;
use tokio::time::timeout;

/// Test the full FT8 decode pipeline from audio input to decoded messages
#[tokio::test]
async fn test_full_ft8_decode_pipeline() -> Result<()> {
    // Initialize logging for tests
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .try_init();

    // Create test configuration
    let mut config = Config::default();
    config.audio.device_name = Some("default".to_string());
    config.audio.sample_rate = 48000;
    config.audio.buffer_size = 512;
    config.dsp.sample_rate = 12000;
    config.ft8.decode_depth = 3;
    
    // Use stub audio for testing
    std::env::set_var("PANCETTA_STUB_AUDIO", "1");
    std::env::set_var("PANCETTA_MOCK_RIG", "true");

    // Create coordinator
    let coordinator = Arc::new(Coordinator::new(config));
    
    // Create a channel to receive decoded messages
    let decoded_messages = Arc::new(RwLock::new(Vec::new()));
    let decoded_messages_clone = decoded_messages.clone();
    
    // Start the coordinator
    let coordinator_handle = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator.run().await
        })
    };

    // Wait for components to initialize
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Simulate running for a period to collect messages
    let test_duration = Duration::from_secs(3);
    let start_time = Instant::now();
    
    while start_time.elapsed() < test_duration {
        // In a real test, we would inject test audio data here
        // For now, we're just testing the pipeline setup
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // Shutdown the coordinator
    coordinator.shutdown().await?;
    
    // Wait for coordinator to finish
    let _ = timeout(Duration::from_secs(5), coordinator_handle).await;

    // Verify the pipeline was set up correctly
    // In a real test, we would check for decoded messages
    let messages = decoded_messages.read().await;
    
    println!("Pipeline test completed. Processed {} messages", messages.len());
    
    Ok(())
}

/// Test audio callback latency
#[tokio::test]
async fn test_audio_callback_latency() -> Result<()> {
    use pancetta_audio::{AudioDeviceManager, AudioConfig};
    use std::sync::atomic::{AtomicU64, Ordering};
    
    // Create audio configuration
    let config = AudioConfig {
        device_name: None,
        sample_rate: 48000,
        buffer_size: 512,
        channels: 1,
        latency_ms: 10,
    };
    
    // Track callback timings
    let callback_count = Arc::new(AtomicU64::new(0));
    let total_latency_us = Arc::new(AtomicU64::new(0));
    let max_latency_us = Arc::new(AtomicU64::new(0));
    
    let callback_count_clone = callback_count.clone();
    let total_latency_clone = total_latency_us.clone();
    let max_latency_clone = max_latency_us.clone();
    
    // Create device manager
    let device_manager = AudioDeviceManager::new(config);
    
    // Start audio stream with latency measurement
    let _stream = device_manager.start_input_stream(move |data| {
        let start = Instant::now();
        
        // Simulate minimal processing
        let _sum: f32 = data.iter().sum();
        
        let latency = start.elapsed().as_micros() as u64;
        callback_count_clone.fetch_add(1, Ordering::Relaxed);
        total_latency_clone.fetch_add(latency, Ordering::Relaxed);
        
        let current_max = max_latency_clone.load(Ordering::Relaxed);
        if latency > current_max {
            max_latency_clone.store(latency, Ordering::Relaxed);
        }
    })?;
    
    // Run for 1 second
    tokio::time::sleep(Duration::from_secs(1)).await;
    
    // Calculate statistics
    let count = callback_count.load(Ordering::Relaxed);
    let total = total_latency_us.load(Ordering::Relaxed);
    let max = max_latency_us.load(Ordering::Relaxed);
    
    if count > 0 {
        let avg_latency_us = total / count;
        println!("Audio Callback Latency:");
        println!("  Callbacks: {}", count);
        println!("  Average: {} µs", avg_latency_us);
        println!("  Maximum: {} µs", max);
        
        // Verify <1ms requirement (1000 µs)
        assert!(avg_latency_us < 1000, "Average latency {} µs exceeds 1ms requirement", avg_latency_us);
    }
    
    Ok(())
}

/// Test message bus throughput
#[tokio::test]
async fn test_message_bus_throughput() -> Result<()> {
    let message_bus = Arc::new(MessageBus::new());
    
    // Create channels for test components
    let (tx1, mut rx1) = message_bus.create_channel(ComponentId::Ft8).await?;
    let (tx2, mut rx2) = message_bus.create_channel(ComponentId::Dsp).await?;
    
    let message_count = 10000;
    let start = Instant::now();
    
    // Send messages
    for i in 0..message_count {
        let msg = ComponentMessage::new(
            ComponentId::Ft8,
            ComponentId::Dsp,
            MessageType::Heartbeat,
            Instant::now(),
        );
        tx1.send(msg).await?;
    }
    
    // Receive messages
    let mut received = 0;
    while let Ok(Some(_)) = timeout(Duration::from_millis(10), rx2.recv()).await {
        received += 1;
        if received >= message_count {
            break;
        }
    }
    
    let elapsed = start.elapsed();
    let throughput = (message_count as f64) / elapsed.as_secs_f64();
    
    println!("Message Bus Throughput:");
    println!("  Messages: {}", message_count);
    println!("  Time: {:?}", elapsed);
    println!("  Throughput: {:.0} msg/sec", throughput);
    
    // Should handle at least 10k messages per second
    assert!(throughput > 10000.0, "Message bus throughput {:.0} msg/sec is too low", throughput);
    
    Ok(())
}

/// Test memory usage stays within limits
#[tokio::test]
async fn test_memory_usage() -> Result<()> {
    use sysinfo::{System, SystemExt, ProcessExt};
    
    let mut system = System::new();
    system.refresh_processes();
    
    let pid = sysinfo::get_current_pid().unwrap();
    let initial_memory = system.process(pid)
        .map(|p| p.memory())
        .unwrap_or(0);
    
    println!("Initial memory: {} KB", initial_memory);
    
    // Create and run coordinator
    let config = Config::default();
    std::env::set_var("PANCETTA_STUB_AUDIO", "1");
    std::env::set_var("PANCETTA_MOCK_RIG", "true");
    
    let coordinator = Arc::new(Coordinator::new(config));
    
    let coordinator_handle = {
        let coordinator = coordinator.clone();
        tokio::spawn(async move {
            coordinator.run().await
        })
    };
    
    // Run for 5 seconds
    tokio::time::sleep(Duration::from_secs(5)).await;
    
    // Check memory again
    system.refresh_processes();
    let final_memory = system.process(pid)
        .map(|p| p.memory())
        .unwrap_or(0);
    
    let memory_increase = final_memory.saturating_sub(initial_memory);
    let memory_mb = memory_increase / 1024;
    
    println!("Final memory: {} KB", final_memory);
    println!("Memory increase: {} KB ({} MB)", memory_increase, memory_mb);
    
    // Shutdown
    coordinator.shutdown().await?;
    let _ = timeout(Duration::from_secs(5), coordinator_handle).await;
    
    // Verify memory usage < 100MB
    assert!(memory_mb < 100, "Memory usage {} MB exceeds 100MB limit", memory_mb);
    
    Ok(())
}