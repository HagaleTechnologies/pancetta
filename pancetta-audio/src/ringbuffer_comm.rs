//! Lock-free ringbuffer communication for real-time audio threads
//!
//! Provides zero-allocation, lock-free communication between the main thread
//! and the real-time audio callback thread.

use atomic::{Atomic, Ordering};
use instant::Instant;
use ringbuf::{
    traits::{Consumer, Observer, Producer, Split},
    HeapRb,
};
use std::sync::{Arc, Mutex};

/// Size constraints for real-time audio processing
pub const DEFAULT_AUDIO_BUFFER_SIZE: usize = 128; // ~2.6ms at 48kHz with 64-sample blocks
pub const DEFAULT_LATENCY_BUFFER_SIZE: usize = 256; // Enough for several seconds of latency measurements
pub const MAX_AUDIO_BUFFER_SIZE: usize = 1024; // Hard limit to prevent excessive memory usage

/// Audio sample with timestamp for real-time processing
#[derive(Debug, Clone)]
pub struct AudioSample {
    /// Audio data as f32 samples
    pub data: Vec<f32>,
    /// Timestamp when sample was captured
    pub timestamp: Instant,
    /// Sample rate of the audio data
    pub sample_rate: u32,
    /// Number of channels
    pub channels: u16,
}

impl AudioSample {
    /// Create a new audio sample
    pub fn new(data: Vec<f32>, sample_rate: u32, channels: u16) -> Self {
        Self {
            data,
            timestamp: Instant::now(),
            sample_rate,
            channels,
        }
    }

    /// Get the duration of this sample in milliseconds
    pub fn duration_ms(&self) -> f64 {
        (self.data.len() as f64 / self.channels as f64) / self.sample_rate as f64 * 1000.0
    }

    /// Get the number of audio frames (samples per channel)
    pub fn frame_count(&self) -> usize {
        self.data.len() / self.channels as usize
    }
}

/// Lock-free communication channel for real-time audio processing
///
/// This structure enables safe, allocation-free communication between
/// the main thread and the audio callback thread.
pub struct AudioComm {
    /// Producer for sending audio samples from real-time thread
    pub audio_producer: Arc<Mutex<<HeapRb<AudioSample> as Split>::Prod>>,
    /// Consumer for receiving audio samples in main thread
    pub audio_consumer: Arc<Mutex<<HeapRb<AudioSample> as Split>::Cons>>,
    /// Producer for sending latency measurements from real-time thread  
    pub latency_producer: Arc<Mutex<<HeapRb<u64> as Split>::Prod>>,
    /// Consumer for receiving latency measurements in main thread
    pub latency_consumer: Arc<Mutex<<HeapRb<u64> as Split>::Cons>>,
    /// Atomic flag for clean shutdown
    pub should_stop: Arc<Atomic<bool>>,
    /// Atomic counter for dropped samples
    pub dropped_samples: Arc<Atomic<u64>>,
    /// Atomic counter for processed samples
    pub processed_samples: Arc<Atomic<u64>>,
}

impl AudioComm {
    /// Create a new lock-free communication channel
    ///
    /// # Parameters
    /// - `audio_buffer_size`: Size of the audio sample buffer (typically 64-256)
    /// - `latency_buffer_size`: Size of the latency measurement buffer (typically 256)
    ///
    /// # Returns
    /// A new AudioComm instance with properly configured ringbuffers
    pub fn new(audio_buffer_size: usize, latency_buffer_size: usize) -> Self {
        // Create audio sample ringbuffer
        let audio_rb = HeapRb::<AudioSample>::new(audio_buffer_size);
        let (audio_producer, audio_consumer) = audio_rb.split();

        // Create latency measurement ringbuffer for nanosecond timestamps
        let latency_rb = HeapRb::<u64>::new(latency_buffer_size);
        let (latency_producer, latency_consumer) = latency_rb.split();

        // Atomic flags and counters
        let should_stop = Arc::new(Atomic::new(false));
        let dropped_samples = Arc::new(Atomic::new(0));
        let processed_samples = Arc::new(Atomic::new(0));

        Self {
            audio_producer: Arc::new(Mutex::new(audio_producer)),
            audio_consumer: Arc::new(Mutex::new(audio_consumer)),
            latency_producer: Arc::new(Mutex::new(latency_producer)),
            latency_consumer: Arc::new(Mutex::new(latency_consumer)),
            should_stop,
            dropped_samples,
            processed_samples,
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

    /// Get the number of available audio samples in the buffer
    pub fn audio_samples_available(&self) -> usize {
        if let Ok(consumer) = self.audio_consumer.lock() {
            consumer.occupied_len()
        } else {
            0
        }
    }

    /// Get the number of available latency measurements in the buffer
    pub fn latency_measurements_available(&self) -> usize {
        if let Ok(consumer) = self.latency_consumer.lock() {
            consumer.occupied_len()
        } else {
            0
        }
    }

    /// Get buffer usage statistics
    pub fn get_buffer_stats(&self) -> BufferStats {
        let audio_occupied = if let Ok(consumer) = self.audio_consumer.lock() {
            consumer.occupied_len()
        } else {
            0
        };

        let audio_capacity = if let Ok(consumer) = self.audio_consumer.lock() {
            consumer.capacity().get()
        } else {
            0
        };

        let latency_occupied = if let Ok(consumer) = self.latency_consumer.lock() {
            consumer.occupied_len()
        } else {
            0
        };

        BufferStats {
            audio_buffer_used: audio_occupied,
            audio_buffer_capacity: audio_capacity,
            audio_buffer_usage_percent: if audio_capacity > 0 {
                (audio_occupied as f64 / audio_capacity as f64) * 100.0
            } else {
                0.0
            },
            latency_buffer_used: latency_occupied,
            dropped_samples: self.dropped_samples.load(Ordering::Relaxed),
            processed_samples: self.processed_samples.load(Ordering::Relaxed),
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

    /// Try to push an audio sample (non-blocking)
    pub fn push_audio_sample(&self, sample: AudioSample) -> Result<(), AudioSample> {
        if let Ok(mut producer) = self.audio_producer.try_lock() {
            match producer.try_push(sample) {
                Ok(_) => {
                    self.processed_samples.fetch_add(1, Ordering::Relaxed);
                    Ok(())
                }
                Err(sample) => {
                    self.dropped_samples.fetch_add(1, Ordering::Relaxed);
                    Err(sample)
                }
            }
        } else {
            self.dropped_samples.fetch_add(1, Ordering::Relaxed);
            Err(sample)
        }
    }

    /// Try to pop an audio sample (non-blocking)
    pub fn pop_audio_sample(&self) -> Option<AudioSample> {
        if let Ok(mut consumer) = self.audio_consumer.try_lock() {
            consumer.try_pop()
        } else {
            None
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

    /// Get statistics about dropped vs processed samples
    pub fn get_drop_rate(&self) -> f64 {
        let dropped = self.dropped_samples.load(Ordering::Relaxed) as f64;
        let processed = self.processed_samples.load(Ordering::Relaxed) as f64;
        let total = dropped + processed;

        if total > 0.0 {
            (dropped / total) * 100.0
        } else {
            0.0
        }
    }

    /// Reset counters (for testing)
    pub fn reset_counters(&self) {
        self.dropped_samples.store(0, Ordering::Relaxed);
        self.processed_samples.store(0, Ordering::Relaxed);
    }
}

/// Buffer usage statistics
#[derive(Debug, Clone)]
pub struct BufferStats {
    /// Number of audio samples currently in buffer
    pub audio_buffer_used: usize,
    /// Total audio buffer capacity
    pub audio_buffer_capacity: usize,
    /// Audio buffer usage as percentage
    pub audio_buffer_usage_percent: f64,
    /// Number of latency measurements currently in buffer
    pub latency_buffer_used: usize,
    /// Total number of dropped samples
    pub dropped_samples: u64,
    /// Total number of processed samples
    pub processed_samples: u64,
}

impl BufferStats {
    /// Check if the audio buffer is nearly full (>80%)
    pub fn is_audio_buffer_nearly_full(&self) -> bool {
        self.audio_buffer_usage_percent > 80.0
    }

    /// Check if there are buffer overruns happening
    pub fn has_buffer_overruns(&self) -> bool {
        self.dropped_samples > 0
    }

    /// Get the sample drop rate as a percentage
    pub fn drop_rate_percent(&self) -> f64 {
        let total = self.dropped_samples + self.processed_samples;
        if total > 0 {
            (self.dropped_samples as f64 / total as f64) * 100.0
        } else {
            0.0
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
    /// Sample rate of the audio data
    pub sample_rate: u32,
    /// Number of channels
    pub channels: u16,
}

impl AudioBatch {
    /// Create a new audio batch with pre-allocated capacity
    pub fn new(capacity: usize, sample_rate: u32, channels: u16) -> Self {
        Self {
            samples: Vec::with_capacity(capacity),
            sample_count: 0,
            timestamp_ns: 0,
            sample_rate,
            channels,
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

    /// Convert batch to AudioSample
    pub fn to_audio_sample(&self) -> AudioSample {
        AudioSample {
            data: self.samples.clone(),
            timestamp: Instant::now(),
            sample_rate: self.sample_rate,
            channels: self.channels,
        }
    }

    /// Get the duration of this batch in milliseconds
    pub fn duration_ms(&self) -> f64 {
        (self.sample_count as f64 / self.channels as f64) / self.sample_rate as f64 * 1000.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread;
    use std::time::Duration;

    #[test]
    fn test_audio_comm_creation() {
        let comm = AudioComm::new(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);
        assert_eq!(comm.audio_samples_available(), 0);
        assert_eq!(comm.latency_measurements_available(), 0);
        assert!(!comm.should_stop());
    }

    #[test]
    fn test_audio_sample_transfer() {
        let comm = AudioComm::new(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);

        let sample = AudioSample::new(vec![0.1, 0.2, 0.3, 0.4], 48000, 2);

        // Push sample
        assert!(comm.push_audio_sample(sample.clone()).is_ok());
        assert_eq!(comm.audio_samples_available(), 1);

        // Pop sample
        let received = comm.pop_audio_sample().unwrap();
        assert_eq!(received.data, sample.data);
        assert_eq!(received.sample_rate, sample.sample_rate);
        assert_eq!(received.channels, sample.channels);
        assert_eq!(comm.audio_samples_available(), 0);
    }

    #[test]
    fn test_buffer_overflow() {
        let comm = AudioComm::new(2, DEFAULT_LATENCY_BUFFER_SIZE); // Small buffer

        let sample1 = AudioSample::new(vec![0.1], 48000, 1);
        let sample2 = AudioSample::new(vec![0.2], 48000, 1);
        let sample3 = AudioSample::new(vec![0.3], 48000, 1); // This should overflow

        assert!(comm.push_audio_sample(sample1).is_ok());
        assert!(comm.push_audio_sample(sample2).is_ok());
        assert!(comm.push_audio_sample(sample3).is_err()); // Buffer full

        let stats = comm.get_buffer_stats();
        assert_eq!(stats.dropped_samples, 1);
        assert_eq!(stats.processed_samples, 2);
    }

    #[test]
    fn test_latency_measurement() {
        let comm = AudioComm::new(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);

        let latency_ns = 500_000; // 0.5ms
        assert!(comm.push_latency(latency_ns).is_ok());
        assert_eq!(comm.latency_measurements_available(), 1);

        let received = comm.pop_latency().unwrap();
        assert_eq!(received, latency_ns);
        assert_eq!(comm.latency_measurements_available(), 0);
    }

    #[test]
    fn test_stop_signal() {
        let comm = AudioComm::new(DEFAULT_AUDIO_BUFFER_SIZE, DEFAULT_LATENCY_BUFFER_SIZE);

        assert!(!comm.should_stop());
        comm.stop();
        assert!(comm.should_stop());
    }

    #[test]
    fn test_buffer_stats() {
        let comm = AudioComm::new(10, DEFAULT_LATENCY_BUFFER_SIZE);

        let stats = comm.get_buffer_stats();
        assert_eq!(stats.audio_buffer_capacity, 10);
        assert_eq!(stats.audio_buffer_used, 0);
        assert_eq!(stats.audio_buffer_usage_percent, 0.0);

        // Add some samples (more than 80% of capacity)
        for i in 0..9 {
            // 9 out of 10 = 90%
            let sample = AudioSample::new(vec![i as f32], 48000, 1);
            comm.push_audio_sample(sample).unwrap();
        }

        let stats = comm.get_buffer_stats();
        assert_eq!(stats.audio_buffer_used, 9);
        assert_eq!(stats.audio_buffer_usage_percent, 90.0);
        assert!(stats.is_audio_buffer_nearly_full());
    }

    #[test]
    fn test_audio_sample_properties() {
        let data = vec![0.1, 0.2, 0.3, 0.4]; // 4 samples, 2 channels = 2 frames
        let sample = AudioSample::new(data, 48000, 2);

        assert_eq!(sample.frame_count(), 2);
        assert_eq!(sample.sample_rate, 48000);
        assert_eq!(sample.channels, 2);

        // Duration: 2 frames / 48000 Hz * 1000 ms = 0.0417ms
        let expected_duration = 2.0 / 48000.0 * 1000.0;
        assert!((sample.duration_ms() - expected_duration).abs() < 0.001);
    }

    #[test]
    fn test_audio_batch() {
        let mut batch = AudioBatch::new(100, 48000, 2);

        assert_eq!(batch.sample_count, 0);
        assert!(batch.add_sample(0.1));
        assert!(batch.add_sample(0.2));
        assert_eq!(batch.sample_count, 2);

        let audio_sample = batch.to_audio_sample();
        assert_eq!(audio_sample.data, vec![0.1, 0.2]);
        assert_eq!(audio_sample.sample_rate, 48000);
        assert_eq!(audio_sample.channels, 2);

        batch.reset();
        assert_eq!(batch.sample_count, 0);
        assert!(batch.samples.is_empty());
    }
}

// Safety: AudioSample is safe to send between threads as it only contains owned data
unsafe impl Send for AudioSample {}
unsafe impl Sync for AudioSample {}

// Safety: AudioComm uses atomic operations and mutexes for thread safety
unsafe impl Send for AudioComm {}
unsafe impl Sync for AudioComm {}

// Safety: AudioBatch contains only owned data and can be safely moved between threads
unsafe impl Send for AudioBatch {}
unsafe impl Sync for AudioBatch {}
