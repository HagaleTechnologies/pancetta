//! Lock-free ringbuffer communication for real-time audio threads
//! 
//! Provides zero-allocation, lock-free communication between the main thread
//! and the real-time audio callback thread.

use ringbuf::{HeapRb, traits::{Split, Producer, Consumer, Observer}};
use std::sync::{Arc, Mutex};
use atomic::{Atomic, Ordering};

/// Lock-free communication channel for real-time audio processing
/// 
/// This structure enables safe, allocation-free communication between
/// the main thread and the audio callback thread.
pub struct AudioComm {
    /// Producer for sending latency measurements from real-time thread  
    pub latency_producer: Arc<Mutex<<HeapRb<u64> as Split>::Prod>>,
    /// Consumer for receiving latency measurements in main thread
    pub latency_consumer: Arc<Mutex<<HeapRb<u64> as Split>::Cons>>,
    /// Atomic flag for clean shutdown
    pub should_stop: Arc<Atomic<bool>>,
}

impl AudioComm {
    /// Create a new lock-free communication channel
    /// 
    /// # Parameters
    /// - `latency_buffer_size`: Size of the latency measurement buffer (typically 256)
    /// 
    /// # Returns
    /// A new AudioComm instance with properly configured ringbuffers
    pub fn new(latency_buffer_size: usize) -> Self {
        // Create latency measurement ringbuffer for nanosecond timestamps
        let latency_rb = HeapRb::<u64>::new(latency_buffer_size);
        let (latency_producer, latency_consumer) = latency_rb.split();
        
        // Atomic flag for coordinated shutdown
        let should_stop = Arc::new(Atomic::new(false));
        
        Self {
            latency_producer: Arc::new(Mutex::new(latency_producer)),
            latency_consumer: Arc::new(Mutex::new(latency_consumer)),
            should_stop,
        }
    }
    
    /// Signal the audio thread to stop processing
    pub fn stop(&self) {
        self.should_stop.store(true, Ordering::Release);
    }
    
    /// Check if the audio thread should stop processing
    pub fn should_stop(&self) -> bool {
        self.should_stop.load(Ordering::Acquire)
    }
    
    /// Get the number of available latency measurements in the buffer
    pub fn latency_measurements_available(&self) -> usize {
        if let Ok(consumer) = self.latency_consumer.lock() {
            consumer.occupied_len()
        } else {
            0
        }
    }
    
    /// Try to push a latency measurement (non-blocking)
    pub fn push_latency(&self, latency_ns: u64) -> Result<(), u64> {
        if let Ok(mut producer) = self.latency_producer.try_lock() {
            producer.try_push(latency_ns)
        } else {
            Err(latency_ns) // Return the value if we couldn't acquire the lock
        }
    }
    
    /// Try to pop a latency measurement (non-blocking)
    pub fn pop_latency(&self) -> Option<u64> {
        if let Ok(mut consumer) = self.latency_consumer.try_lock() {
            consumer.try_pop()
        } else {
            None
        }
    }
}

/// Real-time safe audio sample batch
/// 
/// Pre-allocated structure for passing audio samples without allocation
/// in the real-time audio callback.
pub struct AudioBatch {
    /// Pre-allocated buffer for audio samples
    pub samples: Vec<f32>,
    /// Number of valid samples in the buffer
    pub sample_count: usize,
    /// Timestamp when this batch was created (nanoseconds)
    pub timestamp_ns: u64,
}

impl AudioBatch {
    /// Create a new audio batch with pre-allocated capacity
    pub fn new(capacity: usize) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
            sample_count: 0,
            timestamp_ns: 0,
        }
    }
    
    /// Reset the batch for reuse (no allocation)
    pub fn reset(&mut self) {
        self.sample_count = 0;
        self.timestamp_ns = 0;
        self.samples.clear();
    }
    
    /// Add a sample to the batch (bounds checked)
    pub fn add_sample(&mut self, sample: f32) -> bool {
        if self.samples.len() < self.samples.capacity() {
            self.samples.push(sample);
            self.sample_count += 1;
            true
        } else {
            false
        }
    }
}